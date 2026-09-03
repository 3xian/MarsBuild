use crate::models::{
    ActiveSession, DashboardStats, HunkPage, HunkRecord, SessionCard, SessionDetail, SessionStatus,
    SessionTokenUsageInfo, SessionUpdatePage, TokenDayPoint, TokenUsageSeries,
};
use crate::session_usage::{
    completed_turn_usage, persist_session_usage_cache, session_token_usage,
    session_token_usage_snapshot, SessionTokenUsage,
};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Cache expensive full-tree `updates.jsonl` scans (window drag / FS storms).
const TOKEN_SERIES_CACHE_TTL: Duration = Duration::from_secs(45);

struct TokenSeriesCache {
    at: Instant,
    window_days: u32,
    value: TokenUsageSeries,
}

fn token_series_cache() -> &'static Mutex<Option<TokenSeriesCache>> {
    static CACHE: OnceLock<Mutex<Option<TokenSeriesCache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

#[derive(Clone)]
struct JsonFileCache {
    modified: Option<SystemTime>,
    len: u64,
    value: Value,
}

fn json_file_cache() -> &'static Mutex<HashMap<PathBuf, JsonFileCache>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, JsonFileCache>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

type SessionLocation = (PathBuf, String);

fn session_dir_cache() -> &'static Mutex<HashMap<String, SessionLocation>> {
    static CACHE: OnceLock<Mutex<HashMap<String, SessionLocation>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session not found: {0}")]
    NotFound(String),
}

pub type Result<T> = std::result::Result<T, SessionError>;

pub fn grok_home() -> PathBuf {
    if let Ok(home) = std::env::var("GROK_HOME") {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grok")
}

pub fn sessions_root() -> PathBuf {
    grok_home().join("sessions")
}

pub fn session_ids_on_disk() -> HashSet<String> {
    let mut ids = HashSet::new();
    let root = sessions_root();
    let Ok(groups) = fs::read_dir(root) else {
        return ids;
    };
    for group in groups.flatten() {
        let Ok(entries) = fs::read_dir(group.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().join("summary.json").is_file() {
                ids.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    ids
}

pub fn read_active_sessions() -> Result<Vec<ActiveSession>> {
    let path = grok_home().join("active_sessions.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let list: Vec<ActiveSession> = serde_json::from_str(&raw)?;
    // Grok only prunes dead PIDs on next launch (`collect_crashed`). Stale
    // entries after crash / kill would keep cards "active" forever — filter here.
    Ok(list
        .into_iter()
        .filter(|s| process_is_alive(s.pid))
        .collect())
}

/// Live pid holding `session_id` in `active_sessions.json`, if it is not `ignore_pid`.
pub fn foreign_active_pid(session_id: &str, ignore_pid: Option<u32>) -> Option<u32> {
    read_active_sessions().ok()?.into_iter().find_map(|s| {
        if s.session_id == session_id && ignore_pid != Some(s.pid) {
            Some(s.pid)
        } else {
            None
        }
    })
}

/// IPC reject payload. Serialized as `{ code, message, pid? }` so the
/// frontend can match a stable code instead of grepping the message.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl CommandError {
    pub const SESSION_OPEN_ELSEWHERE: &'static str = "session_open_elsewhere";

    pub fn other(message: impl Into<String>) -> Self {
        Self {
            code: "error".into(),
            message: message.into(),
            pid: None,
        }
    }

    pub fn session_open_elsewhere(pid: u32) -> Self {
        Self {
            code: Self::SESSION_OPEN_ELSEWHERE.into(),
            pid: Some(pid),
            message: format!(
                "session is already open in another Grok process (pid {pid}); close that process or pick a different task"
            ),
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CommandError {}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self::other(message)
    }
}

impl From<&str> for CommandError {
    fn from(message: &str) -> Self {
        Self::other(message)
    }
}

pub fn session_open_elsewhere_error(
    session_id: &str,
    ignore_pid: Option<u32>,
) -> Option<CommandError> {
    foreign_active_pid(session_id, ignore_pid).map(CommandError::session_open_elsewhere)
}

/// Poll until `pid` is gone or `timeout` elapses. Used after killing an ACP
/// child so `active_sessions.json` does not still list it as a foreign owner.
pub fn wait_until_dead(pid: u32, timeout: Duration) -> bool {
    if pid == 0 || !process_is_alive(pid) {
        return true;
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(50));
        if !process_is_alive(pid) {
            return true;
        }
    }
    !process_is_alive(pid)
}

/// Best-effort check that `pid` still refers to a live process.
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        // PROCESS_QUERY_LIMITED_INFORMATION
        const ACCESS: u32 = 0x1000;
        const STILL_ACTIVE: u32 = 259;
        extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
            fn CloseHandle(handle: isize) -> i32;
            fn GetExitCodeProcess(handle: isize, code: *mut u32) -> i32;
        }
        let handle = unsafe { OpenProcess(ACCESS, 0, pid) };
        if handle == 0 {
            return false;
        }
        let mut code = 0u32;
        let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
        unsafe {
            let _ = CloseHandle(handle);
        }
        ok != 0 && code == STILL_ACTIVE
    }
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(target_os = "macos")]
    {
        // `kill -0` succeeds if the process exists (or EPERM — still alive).
        use std::process::{Command, Stdio};
        match Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => true,
            // Exit 1 can be "no such process" or EPERM; check /bin/ps as fallback.
            _ => Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "pid="])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .map(|o| o.status.success() && !o.stdout.is_empty())
                .unwrap_or(false),
        }
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        true
    }
}

fn load_json_value(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        json_file_cache().lock().remove(path);
        return Ok(None);
    }
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified().ok();
    if let Some(cached) = json_file_cache().lock().get(path) {
        if cached.len == metadata.len() && cached.modified == modified {
            return Ok(Some(cached.value.clone()));
        }
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&raw)?;
    json_file_cache().lock().insert(
        path.to_path_buf(),
        JsonFileCache {
            modified,
            len: metadata.len(),
            value: value.clone(),
        },
    );
    Ok(Some(value))
}

/// Load the files needed for a session card without letting one corrupt session
/// make the entire dashboard unavailable. Summary is required; signals are optional.
fn load_session_metadata(dir: &Path) -> Option<(Value, Option<Value>)> {
    let summary_path = dir.join("summary.json");
    let summary = match load_json_value(&summary_path) {
        Ok(Some(value)) => value,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                path = %summary_path.display(),
                error = %error,
                "skipping invalid session summary"
            );
            return None;
        }
    };

    Some((summary, load_session_signals(dir)))
}

fn load_session_signals(dir: &Path) -> Option<Value> {
    let signals_path = dir.join("signals.json");
    match load_json_value(&signals_path) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                path = %signals_path.display(),
                error = %error,
                "ignoring invalid session signals"
            );
            None
        }
    }
}

fn summary_is_hidden(summary: &Value) -> bool {
    if let Some(hidden) = summary.get("hidden").and_then(Value::as_bool) {
        return hidden;
    }
    str_field(summary, &["sessionKind", "session_kind"])
        .is_some_and(|kind| kind.starts_with("subagent"))
}

fn u64_field(v: &Value, keys: &[&str]) -> u64 {
    for key in keys {
        if let Some(n) = v.get(*key).and_then(|x| x.as_u64()) {
            return n;
        }
        if let Some(n) = v.get(*key).and_then(|x| x.as_f64()) {
            return n as u64;
        }
    }
    0
}

fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = v.get(*key).and_then(|x| x.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

fn tools_used(signals: &Value) -> Vec<String> {
    signals
        .get("toolsUsed")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn build_card(
    id: &str,
    cwd: &str,
    summary: &Value,
    signals: Option<&Value>,
    active: Option<&ActiveSession>,
    token_usage: SessionTokenUsage,
    token_usage_pending: bool,
) -> SessionCard {
    let title = str_field(summary, &["generated_title", "session_summary", "title"])
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            // Suffix is more distinctive than UUID prefix (UUIDs share version/variant bits).
            let n = id.chars().count();
            let suffix: String = if n <= 6 {
                id.to_string()
            } else {
                id.chars().skip(n - 6).collect()
            };
            format!("Session: {suffix}")
        });

    let error_count = signals.map(|s| u64_field(s, &["errorCount"])).unwrap_or(0);
    let status = if active.is_some() {
        if error_count > 0 {
            SessionStatus::Error
        } else {
            SessionStatus::Active
        }
    } else if error_count > 0 {
        SessionStatus::Error
    } else {
        SessionStatus::Idle
    };

    SessionCard {
        id: id.to_string(),
        cwd: cwd.to_string(),
        title,
        model_id: str_field(summary, &["current_model_id"]),
        agent_name: str_field(summary, &["agent_name"]),
        head_branch: str_field(summary, &["head_branch"]),
        created_at: str_field(summary, &["created_at"]),
        updated_at: str_field(summary, &["updated_at"]),
        last_active_at: str_field(summary, &["last_active_at"]),
        num_messages: u64_field(summary, &["num_messages"]),
        is_active: active.is_some(),
        active_pid: active.map(|a| a.pid),
        status,
        context_tokens_used: signals
            .map(|s| u64_field(s, &["contextTokensUsed"]))
            .unwrap_or(0),
        context_window_tokens: signals
            .map(|s| u64_field(s, &["contextWindowTokens"]))
            .unwrap_or(0),
        context_window_usage: signals
            .map(|s| u64_field(s, &["contextWindowUsage"]))
            .unwrap_or(0),
        total_tokens: token_usage.total_tokens,
        token_usage_incomplete: token_usage.incomplete,
        token_usage_available: token_usage.available,
        token_usage_pending,
        tool_call_count: signals
            .map(|s| u64_field(s, &["toolCallCount"]))
            .unwrap_or(0),
        turn_count: signals.map(|s| u64_field(s, &["turnCount"])).unwrap_or(0),
        tools_used: signals.map(tools_used).unwrap_or_default(),
        agent_lines_added: signals
            .map(|s| u64_field(s, &["agentLinesAdded"]))
            .unwrap_or(0),
        agent_lines_removed: signals
            .map(|s| u64_field(s, &["agentLinesRemoved"]))
            .unwrap_or(0),
        agent_files_touched: signals
            .map(|s| u64_field(s, &["agentFilesTouched"]))
            .unwrap_or(0),
        session_duration_seconds: signals
            .map(|s| u64_field(s, &["sessionDurationSeconds"]))
            .unwrap_or(0),
        error_count,
    }
}

fn decode_cwd_dir_name(name: &str) -> String {
    urlencoding::decode(name)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| name.to_string())
}

fn find_session_dir(session_id: &str) -> Result<(PathBuf, String)> {
    let cached = { session_dir_cache().lock().get(session_id).cloned() };
    if let Some(cached) = cached {
        if cached.0.is_dir() {
            return Ok(cached);
        }
        session_dir_cache().lock().remove(session_id);
    }

    let root = sessions_root();
    if !root.exists() {
        return Err(SessionError::NotFound(session_id.to_string()));
    }

    for group in fs::read_dir(&root)? {
        let group = group?;
        if !group.file_type()?.is_dir() {
            continue;
        }
        let group_name = group.file_name().to_string_lossy().to_string();
        if group_name == "session_search.sqlite" || group_name.starts_with('.') {
            continue;
        }
        let session_path = group.path().join(session_id);
        if session_path.is_dir() {
            let cwd = if group.path().join(".cwd").exists() {
                fs::read_to_string(group.path().join(".cwd"))
                    .unwrap_or_else(|_| decode_cwd_dir_name(&group_name))
                    .trim()
                    .to_string()
            } else {
                decode_cwd_dir_name(&group_name)
            };
            let location = (session_path, cwd);
            session_dir_cache()
                .lock()
                .insert(session_id.to_string(), location.clone());
            return Ok(location);
        }
    }

    Err(SessionError::NotFound(session_id.to_string()))
}

/// Read Grok session `plan.md` if present (plan mode artifact).
pub fn read_session_plan(session_id: &str) -> Result<Option<SessionPlan>> {
    let (dir, _) = find_session_dir(session_id)?;
    let path = dir.join("plan.md");
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&path)?;
    let empty = content.trim().is_empty();
    Ok(Some(SessionPlan {
        path: path.display().to_string(),
        content,
        empty,
    }))
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPlan {
    pub path: String,
    pub content: String,
    pub empty: bool,
}

/// Resolve a path under a Grok session directory (e.g. generated `images/1.jpg`).
///
/// Accepts project-relative paths or absolute paths that stay inside the session
/// folder. Used by the workspace preview when files live in `~/.grok/sessions/…`
/// rather than the project cwd.
pub fn resolve_in_session(session_id: &str, path: &str) -> std::result::Result<PathBuf, String> {
    let (session_dir, _) = find_session_dir(session_id).map_err(|e| e.to_string())?;
    let session_dir = fs::canonicalize(&session_dir).unwrap_or(session_dir);
    let raw = path.trim();
    if raw.is_empty() {
        return Err("empty path".into());
    }
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        // Normalize mixed separators so Windows joins `images/1.jpg` cleanly.
        let normalized = raw.replace('/', std::path::MAIN_SEPARATOR_STR);
        session_dir.join(normalized)
    };
    let canon = fs::canonicalize(&candidate)
        .map_err(|_| format!("Path does not exist: {}", candidate.to_string_lossy()))?;
    if !canon.starts_with(&session_dir) {
        // Also compare without Windows `\\?\` prefixes.
        let strip = |p: &Path| {
            let s = p.to_string_lossy();
            if let Some(rest) = s.strip_prefix(r"\\?\") {
                PathBuf::from(rest)
            } else {
                PathBuf::from(s.as_ref())
            }
        };
        if !strip(&canon).starts_with(strip(&session_dir)) {
            return Err("path escapes session directory".into());
        }
    }
    Ok(canon)
}

struct SessionCandidate {
    id: String,
    dir: PathBuf,
    cwd: String,
    active: bool,
    modified: SystemTime,
}

fn collect_session_candidates(
    root: &Path,
    active_map: &HashMap<String, ActiveSession>,
) -> Result<Vec<SessionCandidate>> {
    let mut candidates = Vec::new();

    for group in fs::read_dir(root)? {
        let group = group?;
        if !group.file_type()?.is_dir() {
            continue;
        }
        let group_name = group.file_name().to_string_lossy().to_string();
        if group_name.starts_with('.') {
            continue;
        }

        let cwd = if group.path().join(".cwd").exists() {
            fs::read_to_string(group.path().join(".cwd"))
                .unwrap_or_else(|_| decode_cwd_dir_name(&group_name))
                .trim()
                .to_string()
        } else {
            decode_cwd_dir_name(&group_name)
        };

        for entry in fs::read_dir(group.path())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let dir = entry.path();
            let summary_path = dir.join("summary.json");
            let Ok(metadata) = fs::metadata(&summary_path) else {
                continue;
            };
            candidates.push(SessionCandidate {
                active: active_map.contains_key(&id),
                id,
                dir,
                cwd: cwd.clone(),
                modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            });
        }
    }

    // Like Grok Build's recent-session path: stat every summary, then parse only
    // the newest bounded candidate set. Continue past hidden/corrupt entries so
    // the caller still receives up to `limit` visible cards.
    candidates.sort_by(|a, b| {
        b.active
            .cmp(&a.active)
            .then_with(|| b.modified.cmp(&a.modified))
    });
    Ok(candidates)
}

fn sort_session_cards(cards: &mut [SessionCard]) {
    cards.sort_by(|a, b| {
        // Active first, then by parsed activity time. RFC3339 strings with
        // different offsets/fraction widths are not chronologically sortable.
        b.is_active.cmp(&a.is_active).then_with(|| {
            let a_ts = a
                .last_active_at
                .as_ref()
                .or(a.updated_at.as_ref())
                .and_then(|value| parse_iso_ish_to_unix(value))
                .unwrap_or(0);
            let b_ts = b
                .last_active_at
                .as_ref()
                .or(b.updated_at.as_ref())
                .and_then(|value| parse_iso_ish_to_unix(value))
                .unwrap_or(0);
            b_ts.cmp(&a_ts)
        })
    });
}

fn active_session_map() -> HashMap<String, ActiveSession> {
    read_active_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|active| (active.session_id.clone(), active))
        .collect()
}

fn summary_matches_query(id: &str, cwd: &str, summary: &Value, query: &str) -> bool {
    id.to_lowercase().contains(query)
        || cwd.to_lowercase().contains(query)
        || str_field(summary, &["generated_title", "session_summary", "title"])
            .is_some_and(|title| title.to_lowercase().contains(query))
        || str_field(summary, &["head_branch"])
            .is_some_and(|branch| branch.to_lowercase().contains(query))
}

pub fn list_sessions(limit: Option<usize>) -> Result<Vec<SessionCard>> {
    let root = sessions_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let active_map = active_session_map();
    let candidates = collect_session_candidates(&root, &active_map)?;

    let mut cards = Vec::new();
    for candidate in candidates {
        let Some((summary, signals)) = load_session_metadata(&candidate.dir) else {
            continue;
        };
        if summary_is_hidden(&summary) {
            continue;
        }
        let updates_path = candidate.dir.join("updates.jsonl");
        let (token_usage, token_usage_pending) = session_token_usage_snapshot(&updates_path);
        let card = build_card(
            &candidate.id,
            &candidate.cwd,
            &summary,
            signals.as_ref(),
            active_map.get(&candidate.id),
            token_usage,
            token_usage_pending,
        );
        // Drop empty system-temp sessions (ACP tests, handshake probes, etc.).
        if crate::session_noise::is_noise_session(&card) {
            continue;
        }
        session_dir_cache()
            .lock()
            .insert(candidate.id, (candidate.dir, candidate.cwd));
        cards.push(card);
        if limit.is_some_and(|n| cards.len() >= n) {
            break;
        }
    }

    sort_session_cards(&mut cards);

    Ok(cards)
}

pub fn search_sessions(query: &str, limit: Option<usize>) -> Result<Vec<SessionCard>> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return list_sessions(limit);
    }
    let root = sessions_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let max = limit.unwrap_or(100);
    let active_map = active_session_map();
    let candidates = collect_session_candidates(&root, &active_map)?;
    let mut cards = Vec::new();

    for candidate in candidates {
        let summary_path = candidate.dir.join("summary.json");
        let summary = match load_json_value(&summary_path) {
            Ok(Some(value)) => value,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    path = %summary_path.display(),
                    error = %error,
                    "skipping invalid session summary during search"
                );
                continue;
            }
        };
        if summary_is_hidden(&summary) {
            continue;
        }
        if !summary_matches_query(&candidate.id, &candidate.cwd, &summary, &query) {
            continue;
        }

        let signals = load_session_signals(&candidate.dir);
        let (token_usage, token_usage_pending) =
            session_token_usage_snapshot(&candidate.dir.join("updates.jsonl"));
        let card = build_card(
            &candidate.id,
            &candidate.cwd,
            &summary,
            signals.as_ref(),
            active_map.get(&candidate.id),
            token_usage,
            token_usage_pending,
        );
        if crate::session_noise::is_noise_session(&card) {
            continue;
        }
        session_dir_cache()
            .lock()
            .insert(candidate.id, (candidate.dir, candidate.cwd));
        cards.push(card);
        if cards.len() >= max {
            break;
        }
    }

    sort_session_cards(&mut cards);
    Ok(cards)
}

pub fn get_session_card(session_id: &str) -> Result<SessionCard> {
    let (dir, cwd) = find_session_dir(session_id)?;
    let summary = load_json_value(&dir.join("summary.json"))?
        .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
    let signals = load_json_value(&dir.join("signals.json"))?;
    let active = read_active_sessions()
        .unwrap_or_default()
        .into_iter()
        .find(|item| item.session_id == session_id);
    let (token_usage, token_usage_pending) =
        session_token_usage_snapshot(&dir.join("updates.jsonl"));
    Ok(build_card(
        session_id,
        &cwd,
        &summary,
        signals.as_ref(),
        active.as_ref(),
        token_usage,
        token_usage_pending,
    ))
}

pub fn session_token_usages(session_ids: &[String]) -> Vec<SessionTokenUsageInfo> {
    let mut results = Vec::with_capacity(session_ids.len());
    for session_id in session_ids {
        let Ok((dir, _)) = find_session_dir(session_id) else {
            continue;
        };
        let usage = session_token_usage(&dir.join("updates.jsonl"));
        results.push(SessionTokenUsageInfo {
            session_id: session_id.clone(),
            total_tokens: usage.total_tokens,
            incomplete: usage.incomplete,
            available: usage.available,
        });
    }
    if !results.is_empty() {
        persist_session_usage_cache();
    }
    results
}

fn read_jsonl_tail(path: &Path, max_lines: usize) -> Result<Vec<Value>> {
    if !path.exists() || max_lines == 0 {
        return Ok(Vec::new());
    }
    const BLOCK_SIZE: usize = 16 * 1024;
    let mut file = fs::File::open(path)?;
    let mut position = file.metadata()?.len();
    let mut buffer = Vec::new();

    loop {
        let read_len = position.min(BLOCK_SIZE as u64) as usize;
        position -= read_len as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0; read_len];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buffer);
        buffer = chunk;

        let text = String::from_utf8_lossy(&buffer);
        let complete_lines = if position > 0 {
            text.lines().skip(1).collect::<Vec<_>>()
        } else {
            text.lines().collect::<Vec<_>>()
        };
        let valid_count = complete_lines
            .iter()
            .filter(|line| serde_json::from_str::<Value>(line.trim()).is_ok())
            .count();
        if position == 0 || valid_count >= max_lines {
            break;
        }
    }

    let text = String::from_utf8_lossy(&buffer);
    let lines: Vec<_> = if position > 0 {
        text.lines().skip(1).collect()
    } else {
        text.lines().collect()
    };
    let mut values: Vec<Value> = lines
        .into_iter()
        .rev()
        .filter_map(|line| serde_json::from_str(line.trim()).ok())
        .take(max_lines)
        .collect();
    values.reverse();
    Ok(values)
}

fn jsonl_values_with_offsets(
    buffer: &[u8],
    buffer_start: u64,
    skip_partial_first_line: bool,
) -> Vec<(u64, Value)> {
    let mut offset = 0usize;
    buffer
        .split_inclusive(|byte| *byte == b'\n')
        .enumerate()
        .filter_map(|(index, line)| {
            let line_start = buffer_start + offset as u64;
            offset += line.len();
            if skip_partial_first_line && index == 0 {
                return None;
            }
            let line = line
                .strip_suffix(b"\n")
                .unwrap_or(line)
                .strip_suffix(b"\r")
                .unwrap_or(line);
            serde_json::from_slice(line)
                .ok()
                .map(|value| (line_start, value))
        })
        .collect()
}

fn read_update_page(
    path: &Path,
    before_cursor: Option<u64>,
    limit: usize,
) -> Result<SessionUpdatePage> {
    if !path.exists() {
        return Ok(SessionUpdatePage {
            updates: Vec::new(),
            next_cursor: None,
            has_more: false,
        });
    }

    const BLOCK_SIZE: usize = 16 * 1024;
    let limit = limit.clamp(1, 1_000);
    let mut file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let end = before_cursor.unwrap_or(file_len).min(file_len);
    let mut position = end;
    let mut buffer = Vec::new();
    let entries = loop {
        let read_len = position.min(BLOCK_SIZE as u64) as usize;
        position -= read_len as u64;
        file.seek(SeekFrom::Start(position))?;
        let mut chunk = vec![0; read_len];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buffer);
        buffer = chunk;

        let entries = jsonl_values_with_offsets(&buffer, position, position > 0);
        if position == 0 || entries.len() > limit {
            break entries;
        }
    };

    let page_start = entries.len().saturating_sub(limit);
    let has_more = page_start > 0;
    let page = &entries[page_start..];
    Ok(SessionUpdatePage {
        updates: page.iter().map(|(_, value)| value.clone()).collect(),
        next_cursor: has_more.then(|| page[0].0),
        has_more,
    })
}

fn parse_hunk(v: &Value) -> HunkRecord {
    HunkRecord {
        hunk_id: str_field(v, &["hunkId", "hunk_id"]),
        file_path: str_field(v, &["filePath", "file_path"]).unwrap_or_else(|| "unknown".into()),
        hunk_start: v
            .get("hunkStart")
            .or_else(|| v.get("hunk_start"))
            .and_then(|x| x.as_u64()),
        hunk_end: v
            .get("hunkEnd")
            .or_else(|| v.get("hunk_end"))
            .and_then(|x| x.as_u64()),
        lines_added: u64_field(v, &["linesAdded", "lines_added"]),
        lines_removed: u64_field(v, &["linesRemoved", "lines_removed"]),
        author_type: str_field(v, &["authorType", "author_type"]),
        session_id: str_field(v, &["sessionId", "session_id"]),
        timestamp: str_field(v, &["timestamp"]),
    }
}

fn read_hunk_page(path: &Path, limit: usize) -> Result<HunkPage> {
    let values = read_jsonl_tail(path, limit.saturating_add(1))?;
    let mut hunks: Vec<HunkRecord> = values.iter().map(parse_hunk).collect();
    hunks.reverse(); // newest first after tail
    let has_more = hunks.len() > limit;
    hunks.truncate(limit);
    Ok(HunkPage { hunks, has_more })
}

pub fn list_hunks(session_id: &str, limit: Option<usize>) -> Result<HunkPage> {
    let (dir, _) = find_session_dir(session_id)?;
    read_hunk_page(&dir.join("hunk_records.jsonl"), limit.unwrap_or(200))
}

pub fn list_session_updates(
    session_id: &str,
    before_cursor: Option<u64>,
    limit: Option<usize>,
) -> Result<SessionUpdatePage> {
    let (dir, _) = find_session_dir(session_id)?;
    read_update_page(
        &dir.join("updates.jsonl"),
        before_cursor,
        limit.unwrap_or(250),
    )
}

pub fn get_session_detail(session_id: &str) -> Result<SessionDetail> {
    let (dir, cwd) = find_session_dir(session_id)?;
    let summary = load_json_value(&dir.join("summary.json"))?
        .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;
    let signals = load_json_value(&dir.join("signals.json"))?;

    let active = read_active_sessions()
        .unwrap_or_default()
        .into_iter()
        .find(|a| a.session_id == session_id);

    let card = build_card(
        session_id,
        &cwd,
        &summary,
        signals.as_ref(),
        active.as_ref(),
        session_token_usage(&dir.join("updates.jsonl")),
        false,
    );
    persist_session_usage_cache();

    // Keep the first paint fast. The UI can request a deeper tail when the
    // user asks for older Timeline entries and preserves that depth on refresh.
    let recent_events = read_jsonl_tail(&dir.join("events.jsonl"), 30)?;
    let update_page = read_update_page(&dir.join("updates.jsonl"), None, 200)?;
    let recent_hunks = list_hunks(session_id, Some(50))?;

    Ok(SessionDetail {
        card,
        summary_raw: summary,
        signals_raw: signals,
        recent_events,
        recent_updates: update_page.updates,
        recent_updates_cursor: update_page.next_cursor,
        recent_updates_has_more: update_page.has_more,
        recent_hunks,
    })
}

pub fn dashboard_stats() -> Result<DashboardStats> {
    let cards = list_sessions(None)?;
    let active = cards.iter().filter(|c| c.is_active).count();
    Ok(DashboardStats {
        total_sessions: cards.len(),
        active_sessions: active,
        total_context_tokens: cards.iter().map(|c| c.context_tokens_used).sum(),
        total_tool_calls: cards.iter().map(|c| c.tool_call_count).sum(),
        total_files_touched: cards.iter().map(|c| c.agent_files_touched).sum(),
        total_lines_added: cards.iter().map(|c| c.agent_lines_added).sum(),
        total_lines_removed: cards.iter().map(|c| c.agent_lines_removed).sum(),
        grok_home: grok_home().display().to_string(),
    })
}

/// Aggregate turn-level token usage from session `updates.jsonl` for the last `window_days`.
///
/// Prefer **fresh input + output** (`input − cachedRead + output`) so re-sent context is not
/// counted as new spend. Falls back to `totalTokens` when breakdown fields are missing.
///
/// Results are cached briefly — a full tree scan is too heavy to run on every FS tick.
pub fn token_usage_series(window_days: u32) -> Result<TokenUsageSeries> {
    let days_u32 = window_days.clamp(1, 31);
    {
        let cache = token_series_cache().lock();
        if let Some(c) = cache.as_ref() {
            if c.window_days == days_u32 && c.at.elapsed() < TOKEN_SERIES_CACHE_TTL {
                return Ok(c.value.clone());
            }
        }
    }
    let value = token_usage_series_uncached(days_u32)?;
    *token_series_cache().lock() = Some(TokenSeriesCache {
        at: Instant::now(),
        window_days: days_u32,
        value: value.clone(),
    });
    Ok(value)
}

fn token_usage_series_uncached(window_days: u32) -> Result<TokenUsageSeries> {
    let days = window_days as usize;
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = 86_400u64;
    let today_index = now_secs / day_secs;
    // Inclusive window: today and previous (days-1) UTC days.
    let start_index = today_index.saturating_sub((days as u64).saturating_sub(1));
    let start_secs = start_index * day_secs;

    let mut by_day: HashMap<u64, DayAgg> = HashMap::new();

    let root = sessions_root();
    if root.exists() {
        for group in fs::read_dir(&root)? {
            let group = group?;
            if !group.file_type()?.is_dir() {
                continue;
            }
            let group_name = group.file_name().to_string_lossy().to_string();
            if group_name.starts_with('.') {
                continue;
            }
            for entry in fs::read_dir(group.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                // Skip clearly stale sessions (summary last activity before window).
                let summary_path = entry.path().join("summary.json");
                if let Ok(Some(summary)) = load_json_value(&summary_path) {
                    if summary_is_hidden(&summary) {
                        continue;
                    }
                    if let Some(ts) = summary_activity_unix(&summary) {
                        if ts + day_secs < start_secs {
                            continue;
                        }
                    }
                }
                let updates = entry.path().join("updates.jsonl");
                if !updates.is_file() {
                    continue;
                }
                accumulate_turn_usage_from_jsonl(&updates, start_secs, &mut by_day)?;
            }
        }
    }

    let mut points = Vec::with_capacity(days);
    let mut total_tokens = 0u64;
    let mut total_turns = 0u64;
    let mut total_cost_usd_ticks = 0u64;
    for i in 0..days as u64 {
        let day_index = start_index + i;
        let agg = by_day.get(&day_index).copied().unwrap_or_default();
        total_tokens = total_tokens.saturating_add(agg.tokens);
        total_turns = total_turns.saturating_add(agg.turns);
        total_cost_usd_ticks = total_cost_usd_ticks.saturating_add(agg.cost_usd_ticks);
        points.push(TokenDayPoint {
            date: day_index_to_ymd(day_index),
            tokens: agg.tokens,
            turns: agg.turns,
            cost_usd_ticks: agg.cost_usd_ticks,
        });
    }

    Ok(TokenUsageSeries {
        days: points,
        total_tokens,
        total_turns,
        total_cost_usd_ticks,
        window_days: days as u32,
    })
}

fn day_index_to_ymd(day_index: u64) -> String {
    let secs = day_index.saturating_mul(86_400);
    let (y, mo, d, _, _, _) = unix_secs_to_utc(secs);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// Howard Hinnant civil-from-days (UTC), shared with agent_manager timestamps.
fn unix_secs_to_utc(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let ss = (secs % 60) as u32;
    let mins = secs / 60;
    let mi = (mins % 60) as u32;
    let hours = mins / 60;
    let h = (hours % 24) as u32;
    let days = (hours / 24) as i64;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (if mo <= 2 { y + 1 } else { y }) as i32;
    (y, mo, d, h, mi, ss)
}

fn summary_activity_unix(summary: &Value) -> Option<u64> {
    for key in ["last_active_at", "updated_at", "created_at"] {
        if let Some(s) = summary.get(key).and_then(|v| v.as_str()) {
            if let Some(secs) = parse_iso_ish_to_unix(s) {
                return Some(secs);
            }
        }
    }
    None
}

/// Best-effort ISO-8601 → unix seconds (handles Z, numeric offsets, and fractions).
fn parse_iso_ish_to_unix(s: &str) -> Option<u64> {
    // Fast path: pure unix seconds / millis as string.
    if let Ok(n) = s.parse::<u64>() {
        return Some(if n > 10_000_000_000 { n / 1000 } else { n });
    }
    let s = s.trim();
    let (s, offset_seconds) = split_iso_offset(s)?;
    let (date, time) = if let Some((d, t)) = s.split_once('T') {
        (d, t)
    } else {
        return None;
    };
    let time = time.split('.').next().unwrap_or(time);
    let mut dp = date.split('-');
    let y: i32 = dp.next()?.parse().ok()?;
    let mo: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    let mut tp = time.split(':');
    let h: u32 = tp.next()?.parse().ok()?;
    let mi: u32 = tp.next()?.parse().ok()?;
    let sec: u32 = tp.next().unwrap_or("0").parse().ok()?;
    let local_as_utc = utc_ymd_hms_to_unix(y, mo, d, h, mi, sec) as i64;
    Some(local_as_utc.saturating_sub(offset_seconds).max(0) as u64)
}

fn split_iso_offset(s: &str) -> Option<(&str, i64)> {
    if let Some(base) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        return Some((base, 0));
    }
    let tpos = s.find('T')?;
    let offset_index = s
        .char_indices()
        .rev()
        .find(|(index, ch)| *index > tpos && (*ch == '+' || *ch == '-'))
        .map(|(index, _)| index);
    let Some(index) = offset_index else {
        // Preserve the previous behavior for timestamps without an explicit zone.
        return Some((s, 0));
    };

    let offset = &s[index..];
    let sign = if offset.starts_with('-') { -1i64 } else { 1i64 };
    let digits = &offset[1..];
    let (hours, minutes) = if let Some((h, m)) = digits.split_once(':') {
        (h, m)
    } else if digits.len() == 4 {
        (&digits[..2], &digits[2..])
    } else {
        return None;
    };
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((&s[..index], sign * (hours * 3600 + minutes * 60)))
}

fn utc_ymd_hms_to_unix(y: i32, mo: u32, d: u32, h: u32, mi: u32, sec: u32) -> u64 {
    // days_from_civil (Hinnant)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if mo > 2 { mo - 3 } else { mo + 9 };
    let doy = (153 * mp as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as i64 * 146_097 + doe as i64 - 719_468;
    let secs = days * 86_400 + (h as i64) * 3600 + (mi as i64) * 60 + sec as i64;
    secs.max(0) as u64
}

/// Cap per-file scan: full-file reads of multi‑MB `updates.jsonl` freeze startup.
const TOKEN_JSONL_MAX_BYTES: u64 = 1_500_000;

#[derive(Clone, Copy, Default)]
struct DayAgg {
    tokens: u64,
    turns: u64,
    cost_usd_ticks: u64,
}

fn accumulate_turn_usage_from_jsonl(
    path: &Path,
    start_secs: u64,
    by_day: &mut HashMap<u64, DayAgg>,
) -> Result<()> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    // Large session logs: only the tail matters for a short usage window.
    if len > TOKEN_JSONL_MAX_BYTES {
        let start = len - TOKEN_JSONL_MAX_BYTES;
        file.seek(SeekFrom::Start(start))?;
    }
    let mut reader = BufReader::new(file);
    // If we seeked mid-file, drop the first partial line.
    if len > TOKEN_JSONL_MAX_BYTES {
        let mut discard = String::new();
        let _ = reader.read_line(&mut discard);
    }
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Cheap filter before full JSON parse.
        if !line.contains("turn_completed") || !line.contains("usage") {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(turn) = completed_turn_usage(&msg) else {
            continue;
        };
        let tokens = turn.fresh_tokens;
        let cost = turn.cost_usd_ticks;
        // Skip empty turns (no tokens and no trusted cost).
        if tokens == 0 && cost == 0 {
            continue;
        }
        let ts = extract_update_unix_secs(&msg).unwrap_or(0);
        if ts < start_secs {
            continue;
        }
        let day_index = ts / 86_400;
        let entry = by_day.entry(day_index).or_default();
        entry.tokens = entry.tokens.saturating_add(tokens);
        entry.turns = entry.turns.saturating_add(1);
        entry.cost_usd_ticks = entry.cost_usd_ticks.saturating_add(cost);
    }
    Ok(())
}

fn extract_update_unix_secs(msg: &Value) -> Option<u64> {
    // Prefer agent wall-clock ms when present.
    if let Some(ms) = msg
        .pointer("/params/update/_meta/agentTimestampMs")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            msg.pointer("/params/_meta/agentTimestampMs")
                .and_then(|v| v.as_u64())
        })
    {
        return Some(if ms > 10_000_000_000 { ms / 1000 } else { ms });
    }
    if let Some(n) = msg.get("timestamp").and_then(|v| v.as_u64()) {
        return Some(if n > 10_000_000_000 { n / 1000 } else { n });
    }
    if let Some(s) = msg.get("timestamp").and_then(|v| v.as_str()) {
        return parse_iso_ish_to_unix(s);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_until_dead_treats_pid_zero_as_gone() {
        assert!(wait_until_dead(0, Duration::from_millis(1)));
    }

    #[test]
    fn exclusive_session_error_uses_stable_code() {
        let err = CommandError::session_open_elsewhere(4321);
        assert_eq!(err.code, CommandError::SESSION_OPEN_ELSEWHERE);
        assert_eq!(err.pid, Some(4321));
        let json = serde_json::to_value(&err).expect("serialize");
        assert_eq!(json["code"], "session_open_elsewhere");
        assert_eq!(json["pid"], 4321);
        assert!(json["message"].as_str().unwrap().contains("4321"));
    }

    #[test]
    fn token_usage_series_returns_window() {
        let s = token_usage_series(7).expect("series");
        assert_eq!(s.days.len(), 7);
        assert_eq!(s.window_days, 7);
        // Dates should be contiguous YYYY-MM-DD
        for p in &s.days {
            assert_eq!(p.date.len(), 10);
            assert!(p.date.chars().nth(4) == Some('-'));
        }
    }

    #[test]
    fn iso_offsets_are_converted_to_utc() {
        assert_eq!(
            parse_iso_ish_to_unix("2026-07-21T00:30:00+08:00"),
            parse_iso_ish_to_unix("2026-07-20T16:30:00Z")
        );
        assert_eq!(
            parse_iso_ish_to_unix("2026-07-20T23:30:00-01:00"),
            parse_iso_ish_to_unix("2026-07-21T00:30:00Z")
        );
        assert_eq!(
            parse_iso_ish_to_unix("2026-07-21T00:30:00+0800"),
            parse_iso_ish_to_unix("2026-07-20T16:30:00Z")
        );
    }

    #[test]
    fn hidden_semantics_match_grok_summary() {
        assert!(summary_is_hidden(&serde_json::json!({"hidden": true})));
        assert!(summary_is_hidden(
            &serde_json::json!({"session_kind": "subagent_worker"})
        ));
        assert!(!summary_is_hidden(
            &serde_json::json!({"hidden": false, "session_kind": "subagent_worker"})
        ));
        assert!(!summary_is_hidden(
            &serde_json::json!({"sessionKind": "interactive"})
        ));
    }

    #[test]
    fn summary_search_matches_only_index_metadata() {
        let summary = serde_json::json!({
            "generated_title": "Repair task startup",
            "head_branch": "codex/task-index"
        });
        assert!(summary_matches_query(
            "session-123",
            r"D:\code\PinkCode",
            &summary,
            "startup"
        ));
        assert!(summary_matches_query(
            "session-123",
            r"D:\code\PinkCode",
            &summary,
            "task-index"
        ));
        assert!(summary_matches_query(
            "session-123",
            r"D:\code\PinkCode",
            &summary,
            "pinkcode"
        ));
        assert!(!summary_matches_query(
            "session-123",
            r"D:\code\PinkCode",
            &summary,
            "tokens"
        ));
    }

    #[test]
    fn corrupt_session_metadata_is_isolated() {
        let dir = std::env::temp_dir().join(format!(
            "pinkcode-session-metadata-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("fixture dir");
        fs::write(dir.join("summary.json"), "{").expect("bad summary");
        assert!(load_session_metadata(&dir).is_none());

        fs::write(dir.join("summary.json"), r#"{"title":"valid"}"#).expect("summary");
        fs::write(dir.join("signals.json"), "{").expect("bad signals");
        let (summary, signals) = load_session_metadata(&dir).expect("valid summary remains usable");
        assert_eq!(summary["title"], "valid");
        assert!(signals.is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn jsonl_tail_reads_last_valid_records_across_blocks() {
        let path = std::env::temp_dir().join(format!(
            "pinkcode-tail-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let mut content = String::new();
        for index in 0..120 {
            content.push_str(
                &serde_json::json!({ "index": index, "padding": "x".repeat(300) }).to_string(),
            );
            content.push('\n');
        }
        fs::write(&path, content).expect("write fixture");

        let tail = read_jsonl_tail(&path, 3).expect("read tail");
        let _ = fs::remove_file(&path);
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0]["index"], 117);
        assert_eq!(tail[2]["index"], 119);
    }

    #[test]
    fn hunk_page_reports_older_records_without_exposing_the_probe_item() {
        let path = std::env::temp_dir().join(format!(
            "pinkcode-hunk-page-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let records = (0..51)
            .map(|index| {
                serde_json::json!({
                    "hunkId": format!("hunk-{index}"),
                    "filePath": format!("src/file-{}.ts", index % 3),
                    "linesAdded": 1,
                    "linesRemoved": 0
                })
                .to_string()
            })
            .collect::<Vec<_>>();

        fs::write(&path, records[..50].join("\n")).expect("write exact page");
        let exact = read_hunk_page(&path, 50).expect("read exact page");
        assert!(!exact.has_more);
        assert_eq!(exact.hunks.len(), 50);
        assert_eq!(exact.hunks[0].hunk_id.as_deref(), Some("hunk-49"));

        fs::write(&path, records.join("\n")).expect("write overflow page");
        let overflow = read_hunk_page(&path, 50).expect("read overflow page");
        let _ = fs::remove_file(&path);
        assert!(overflow.has_more);
        assert_eq!(overflow.hunks.len(), 50);
        assert_eq!(overflow.hunks[0].hunk_id.as_deref(), Some("hunk-50"));
        assert_eq!(overflow.hunks[49].hunk_id.as_deref(), Some("hunk-1"));
    }

    #[test]
    fn update_tail_reports_and_reveals_older_records() {
        let path = std::env::temp_dir().join(format!(
            "pinkcode-update-tail-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let content = (0..250)
            .map(|index| serde_json::json!({ "index": index }).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content).expect("write fixture");

        let first_page = read_update_page(&path, None, 200).expect("first page");
        assert!(first_page.has_more);
        assert_eq!(first_page.updates.len(), 200);
        assert_eq!(first_page.updates[0]["index"], 50);

        let older = read_update_page(&path, first_page.next_cursor, 250).expect("older page");
        let _ = fs::remove_file(&path);
        assert!(!older.has_more);
        assert_eq!(older.updates.len(), 50);
        assert_eq!(older.updates[0]["index"], 0);
        assert_eq!(older.updates[49]["index"], 49);
    }

    #[test]
    fn lists_local_sessions_when_available() {
        let cards = list_sessions(Some(5)).expect("list sessions");
        let stats = dashboard_stats().expect("stats");
        assert!(stats.total_sessions >= cards.len());
        if let Some(first) = cards.first() {
            assert!(!first.id.is_empty());
        }
    }

    #[test]
    fn list_sessions_hides_empty_system_temp() {
        let cards = list_sessions(None).expect("list");
        for c in &cards {
            if crate::session_noise::is_system_temp_cwd(&c.cwd) {
                assert!(
                    c.is_active
                        || c.num_messages > 0
                        || c.tool_call_count > 0
                        || c.context_tokens_used > 0,
                    "empty temp session should be filtered: {} {}",
                    c.id,
                    c.cwd
                );
            }
        }
    }

    #[test]
    fn loads_detail_for_first_session_when_available() {
        let cards = list_sessions(Some(1)).expect("list");
        if let Some(card) = cards.first() {
            let detail = get_session_detail(&card.id).expect("detail");
            assert_eq!(detail.card.id, card.id);
        }
    }
}
