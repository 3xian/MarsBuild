mod acp;
mod agent_fs;
mod agent_manager;
mod agent_runtime;
mod agent_types;
mod ask_user_question;
mod auth;
mod billing;
mod cli;
mod config;
mod fs_atomic;
mod json_util;
mod models;
mod permission_policy;
mod plan_approval;
mod plan_file_policy;
mod project_fs;
mod proxy;
mod rpc_handler;
mod session_noise;
mod session_usage;
mod sessions;
mod shell_emitter;
mod shell_stream;
mod skill_catalog;
mod task_prefs;
mod watcher;
#[cfg(windows)]
mod windows_icons;

use agent_manager::AgentManager;
use agent_types::{
    AttachRequest, ManagedAgentInfo, PendingPermission, PermissionMode, ResolvePermissionRequest,
    SpawnRequest,
};
use billing::WeekUsage;
use models::{
    ActiveSession, DashboardStats, HunkPage, SessionCard, SessionDetail, SessionTokenUsageInfo,
    SessionUpdatePage, TokenUsageSeries,
};
use project_fs::{DirEntry, GitBranchInfo, GitChange, GitFileDiff};
use serde_json::Value;
use skill_catalog::AvailableCommandInfo;
use std::collections::HashMap;
use std::path::Path;
use tauri::Manager;

#[tauri::command]
fn get_grok_home() -> String {
    sessions::grok_home().display().to_string()
}

#[tauri::command]
async fn list_active_sessions() -> Result<Vec<ActiveSession>, String> {
    tauri::async_runtime::spawn_blocking(sessions::read_active_sessions)
        .await
        .map_err(|e| format!("session scan task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_sessions(limit: Option<usize>) -> Result<Vec<SessionCard>, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::list_sessions(limit))
        .await
        .map_err(|e| format!("session scan task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn search_sessions(query: String, limit: Option<usize>) -> Result<Vec<SessionCard>, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::search_sessions(&query, limit))
        .await
        .map_err(|e| format!("session search task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_session_card(session_id: String) -> Result<SessionCard, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::get_session_card(&session_id))
        .await
        .map_err(|e| format!("session card task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_session_token_usages(
    session_ids: Vec<String>,
) -> Result<Vec<SessionTokenUsageInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::session_token_usages(&session_ids))
        .await
        .map_err(|e| format!("session usage task failed: {e}"))
}

#[tauri::command]
async fn get_session_detail(session_id: String) -> Result<SessionDetail, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::get_session_detail(&session_id))
        .await
        .map_err(|e| format!("session detail task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_session_updates(
    session_id: String,
    before_cursor: Option<u64>,
    limit: Option<usize>,
) -> Result<SessionUpdatePage, String> {
    tauri::async_runtime::spawn_blocking(move || {
        sessions::list_session_updates(&session_id, before_cursor, limit)
    })
    .await
    .map_err(|e| format!("session updates task failed: {e}"))?
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_session_plan(session_id: String) -> Result<Option<sessions::SessionPlan>, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::read_session_plan(&session_id))
        .await
        .map_err(|e| format!("session plan task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn list_session_hunks(session_id: String, limit: Option<usize>) -> Result<HunkPage, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::list_hunks(&session_id, limit))
        .await
        .map_err(|e| format!("hunk scan task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_dashboard_stats() -> Result<DashboardStats, String> {
    tauri::async_runtime::spawn_blocking(sessions::dashboard_stats)
        .await
        .map_err(|e| format!("dashboard scan task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_token_usage_series(days: Option<u32>) -> Result<TokenUsageSeries, String> {
    tauri::async_runtime::spawn_blocking(move || sessions::token_usage_series(days.unwrap_or(7)))
        .await
        .map_err(|e| format!("token scan task failed: {e}"))?
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_week_usage() -> WeekUsage {
    // Network I/O off the main thread so the UI stays responsive.
    tauri::async_runtime::spawn_blocking(billing::fetch_week_usage)
        .await
        .unwrap_or_else(|e| billing::WeekUsage {
            used_percent: 0.0,
            remaining_percent: 100.0,
            build_used_percent: None,
            build_remaining_percent: None,
            period_type: "unknown".into(),
            period_start: None,
            period_end: None,
            product_usage: vec![],
            fetched_at: String::new(),
            error: Some(format!("Usage fetch task failed: {e}")),
        })
}

#[tauri::command]
fn resolve_grok_bin(manager: tauri::State<'_, AgentManager>) -> Result<String, String> {
    manager.resolve_grok_bin()
}

#[tauri::command]
async fn list_slash_skills(
    manager: tauri::State<'_, AgentManager>,
    cwd: String,
) -> Result<Vec<AvailableCommandInfo>, String> {
    let grok_bin = manager.resolve_grok_bin()?;
    tauri::async_runtime::spawn_blocking(move || skill_catalog::list_slash_skills(&grok_bin, &cwd))
        .await
        .map_err(|error| format!("skill discovery task failed: {error}"))?
}

#[tauri::command]
fn list_managed_agents(manager: tauri::State<'_, AgentManager>) -> Vec<ManagedAgentInfo> {
    manager.list()
}

#[tauri::command]
async fn spawn_agent(
    manager: tauri::State<'_, AgentManager>,
    request: SpawnRequest,
) -> Result<ManagedAgentInfo, String> {
    // ACP spawn/init is blocking I/O — keep it off the UI/command hot path.
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.spawn(request))
        .await
        .map_err(|e| format!("spawn task failed: {e}"))?
}

#[tauri::command]
async fn attach_agent(
    manager: tauri::State<'_, AgentManager>,
    request: AttachRequest,
) -> Result<ManagedAgentInfo, String> {
    // session/load can take seconds; run off-thread so the webview keeps painting
    // (breathing light on the task card, etc.).
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.attach(request))
        .await
        .map_err(|e| format!("attach task failed: {e}"))?
}

#[tauri::command]
async fn prompt_agent(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    text: String,
) -> Result<Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.prompt(&handle_id, &text))
        .await
        .map_err(|e| format!("prompt task failed: {e}"))?
}

#[tauri::command]
async fn interject_agent(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    text: String,
) -> Result<Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.interject(&handle_id, &text))
        .await
        .map_err(|e| format!("interject task failed: {e}"))?
}

#[tauri::command]
fn queue_remove(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    id: String,
    version: u64,
) -> Result<(), String> {
    manager.queue_remove(&handle_id, &id, version)
}

#[tauri::command]
fn queue_reorder(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    ordered_ids: Vec<String>,
) -> Result<(), String> {
    manager.queue_reorder(&handle_id, &ordered_ids)
}

#[tauri::command]
fn queue_clear(manager: tauri::State<'_, AgentManager>, handle_id: String) -> Result<(), String> {
    manager.queue_clear(&handle_id)
}

#[tauri::command]
fn queue_edit(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    id: String,
    new_text: String,
) -> Result<(), String> {
    manager.queue_edit(&handle_id, &id, &new_text)
}

#[tauri::command]
fn queue_interject(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    id: String,
    version: u64,
) -> Result<(), String> {
    manager.queue_interject(&handle_id, &id, version)
}

#[tauri::command]
async fn stop_agent(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
) -> Result<ManagedAgentInfo, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.stop(&handle_id))
        .await
        .map_err(|e| format!("stop task failed: {e}"))?
}

#[tauri::command]
fn get_managed_agent(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
) -> Option<ManagedAgentInfo> {
    manager.get(&handle_id)
}

#[tauri::command]
fn find_managed_by_session(
    manager: tauri::State<'_, AgentManager>,
    session_id: String,
) -> Option<ManagedAgentInfo> {
    manager.find_by_session(&session_id)
}

#[tauri::command]
fn list_pending_permissions(
    manager: tauri::State<'_, AgentManager>,
    handle_id: Option<String>,
) -> Vec<PendingPermission> {
    manager.list_pending_permissions(handle_id)
}

#[tauri::command]
fn resolve_permission(
    manager: tauri::State<'_, AgentManager>,
    request: ResolvePermissionRequest,
) -> Result<PendingPermission, String> {
    manager.resolve_permission(request)
}

#[tauri::command]
fn set_permission_mode(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    mode: PermissionMode,
) -> Result<ManagedAgentInfo, String> {
    manager.set_permission_mode(&handle_id, mode)
}

/// ACP `session/set_mode` (e.g. `"plan"`). Required for real plan mode over ACP.
#[tauri::command]
async fn set_session_mode(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    mode_id: String,
) -> Result<(), String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.set_session_mode(&handle_id, &mode_id))
        .await
        .map_err(|e| format!("set_session_mode task failed: {e}"))?
}

/// ACP `session/set_model` — switch the model for a session.
#[tauri::command]
async fn set_session_model(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    model_id: String,
    reasoning_effort: Option<String>,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        manager.set_session_model(&handle_id, &model_id, reasoning_effort.as_deref())
    })
    .await
    .map_err(|e| format!("set_session_model task failed: {e}"))?
}

/// ACP `x.ai/session/usage` — cumulative session tokens and cost.
#[tauri::command]
async fn get_session_usage(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.session_usage(&handle_id))
        .await
        .map_err(|e| format!("session_usage task failed: {e}"))?
}

/// ACP `x.ai/recap` — request recap; body arrives as `session_recap` on the timeline.
#[tauri::command]
async fn get_session_recap(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    auto: Option<bool>,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.recap(&handle_id, auto.unwrap_or(false)))
        .await
        .map_err(|e| format!("recap task failed: {e}"))?
}

/// ACP `x.ai/rewind/points` — list rewindable prompt indices.
#[tauri::command]
async fn get_rewind_points(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.rewind_points(&handle_id))
        .await
        .map_err(|e| format!("rewind_points task failed: {e}"))?
}

/// ACP `x.ai/rewind/execute` — rewind session to a previous prompt index.
#[tauri::command]
async fn rewind_execute(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    target_prompt_index: u64,
    mode: Option<String>,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        manager.rewind_execute(&handle_id, target_prompt_index, mode.as_deref())
    })
    .await
    .map_err(|e| format!("rewind_execute task failed: {e}"))?
}

/// ACP `x.ai/subagent/cancel` — cancel a running subagent.
#[tauri::command]
async fn cancel_subagent(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    subagent_id: String,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.cancel_subagent(&handle_id, &subagent_id))
        .await
        .map_err(|e| format!("cancel_subagent task failed: {e}"))?
}

/// ACP `x.ai/subagent/list_running` — list running subagents.
#[tauri::command]
async fn list_subagents(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.list_subagents(&handle_id))
        .await
        .map_err(|e| format!("list_subagents task failed: {e}"))?
}

/// ACP `x.ai/task/kill` — kill a background task.
#[tauri::command]
async fn kill_task(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
    task_id: String,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.kill_task(&handle_id, &task_id))
        .await
        .map_err(|e| format!("kill_task task failed: {e}"))?
}

/// ACP `x.ai/task/list` — list background tasks.
#[tauri::command]
async fn list_tasks(
    manager: tauri::State<'_, AgentManager>,
    handle_id: String,
) -> Result<serde_json::Value, String> {
    let manager = manager.inner().clone();
    tauri::async_runtime::spawn_blocking(move || manager.list_tasks(&handle_id))
        .await
        .map_err(|e| format!("list_tasks task failed: {e}"))?
}

/// Persist permission mode for a task even when no agent is attached.
#[tauri::command]
fn set_task_permission_mode(session_id: String, mode: PermissionMode) -> Result<(), String> {
    task_prefs::set_permission_mode(&session_id, mode)
}

#[tauri::command]
fn get_task_permission_mode(session_id: String) -> Option<PermissionMode> {
    task_prefs::get_permission_mode(&session_id)
}

#[tauri::command]
fn list_task_permission_modes() -> HashMap<String, PermissionMode> {
    task_prefs::all_permission_modes()
}

/// Default permission for New Task / seed. Optional `project_cwd` pulls in
/// project-layer config when the modal knows a workspace root.
#[tauri::command]
fn get_last_spawn_permission_mode(project_cwd: Option<String>) -> PermissionMode {
    let cwd = project_cwd
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Path::new);
    task_prefs::effective_permission_mode(None, cwd)
}

#[tauri::command]
fn set_task_plan_armed(session_id: String, armed: bool) -> Result<(), String> {
    task_prefs::set_plan_armed(&session_id, armed)
}

#[tauri::command]
fn get_task_plan_armed(session_id: String) -> bool {
    task_prefs::get_plan_armed(&session_id)
}

#[tauri::command]
fn list_task_plan_armed() -> HashMap<String, bool> {
    task_prefs::all_plan_armed()
}

#[tauri::command]
async fn list_project_dir(root: String, path: Option<String>) -> Result<Vec<DirEntry>, String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::list_dir(&root, path.as_deref()))
        .await
        .map_err(|e| format!("directory scan task failed: {e}"))?
}

#[tauri::command]
async fn open_project_path(
    root: String,
    path: String,
    session_id: Option<String>,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        project_fs::open_path(&root, &path, session_id.as_deref())
    })
    .await
    .map_err(|e| format!("open path task failed: {e}"))?
}

#[tauri::command]
async fn read_project_file(
    root: String,
    path: String,
    session_id: Option<String>,
) -> Result<project_fs::FilePreview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        project_fs::read_file(&root, &path, session_id.as_deref())
    })
    .await
    .map_err(|e| format!("read file task failed: {e}"))?
}

#[tauri::command]
async fn git_status(cwd: String) -> Result<Vec<GitChange>, String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_status(&cwd))
        .await
        .map_err(|e| format!("git status task failed: {e}"))?
}

/// Git branch info: name, upstream, ahead/behind, staged/unstaged/untracked counts.
#[tauri::command]
async fn git_branch_info(cwd: String) -> Result<GitBranchInfo, String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_branch_info(&cwd))
        .await
        .map_err(|e| format!("git branch info task failed: {e}"))?
}

/// Git unified diff for a single file (staged or unstaged).
#[tauri::command]
async fn git_diff_file(cwd: String, path: String, staged: bool) -> Result<GitFileDiff, String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_diff_file(&cwd, &path, staged))
        .await
        .map_err(|e| format!("git diff file task failed: {e}"))?
}

/// Stage a file (git add).
#[tauri::command]
async fn git_stage_file(cwd: String, path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_stage(&cwd, &path))
        .await
        .map_err(|e| format!("git stage task failed: {e}"))?
}

/// Unstage a file (git reset HEAD).
#[tauri::command]
async fn git_unstage_file(cwd: String, path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_unstage(&cwd, &path))
        .await
        .map_err(|e| format!("git unstage task failed: {e}"))?
}

/// Stage all changes.
#[tauri::command]
async fn git_stage_all(cwd: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_stage_all(&cwd))
        .await
        .map_err(|e| format!("git stage all task failed: {e}"))?
}

/// Unstage all changes.
#[tauri::command]
async fn git_unstage_all(cwd: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_unstage_all(&cwd))
        .await
        .map_err(|e| format!("git unstage all task failed: {e}"))?
}

/// Commit staged changes.
#[tauri::command]
async fn git_commit(cwd: String, message: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_commit(&cwd, &message))
        .await
        .map_err(|e| format!("git commit task failed: {e}"))?
}

/// Apply a partial unified-diff patch to the index (hunk stage/unstage).
#[tauri::command]
async fn git_apply_patch(cwd: String, patch: String, reverse: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || project_fs::git_apply_patch(&cwd, &patch, reverse))
        .await
        .map_err(|e| format!("git apply patch task failed: {e}"))?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Layered config + structured logging before any agent/watcher work.
    config::init_tracing();

    let flags = match cli::parse(std::env::args()) {
        cli::Action::Help => {
            print!("{}", cli::USAGE);
            return;
        }
        cli::Action::Run(flags) => flags,
    };
    // GTK_CSD must be set before gtk_init inside Builder::run.
    flags.prepare_env();

    let mut context = tauri::generate_context!();
    flags.apply(context.config_mut());

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .manage(AgentManager::new())
        .setup(|app| {
            #[cfg(desktop)]
            {
                app.handle()
                    .plugin(tauri_plugin_updater::Builder::new().build())?;
            }
            // Version only at runtime so conf/html stay product name (no version drift).
            if let Some(window) = app.get_webview_window("main") {
                #[cfg(windows)]
                window.set_decorations(false)?;
                let name = app
                    .config()
                    .product_name
                    .clone()
                    .unwrap_or_else(|| app.package_info().name.clone());
                let version = app.package_info().version.to_string();
                let _ = window.set_title(&format!("{name} {version}"));
            }
            // Tauri's default window icon is a single ICO frame; re-apply native
            // multi-size PE icons so title bar + taskbar stay crisp on high DPI.
            #[cfg(windows)]
            windows_icons::apply_to_app(app);
            let manager = app.state::<AgentManager>();
            manager.set_app(app.handle().clone());
            // Disk-driven session index: FS events + debounce (no fixed 4s poll).
            watcher::start(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_grok_home,
            list_active_sessions,
            list_sessions,
            search_sessions,
            get_session_card,
            list_session_token_usages,
            get_session_detail,
            list_session_updates,
            get_session_plan,
            list_session_hunks,
            get_dashboard_stats,
            get_token_usage_series,
            get_week_usage,
            resolve_grok_bin,
            list_slash_skills,
            list_managed_agents,
            spawn_agent,
            attach_agent,
            prompt_agent,
            interject_agent,
            queue_remove,
            queue_reorder,
            queue_clear,
            queue_edit,
            queue_interject,
            stop_agent,
            get_managed_agent,
            find_managed_by_session,
            list_pending_permissions,
            resolve_permission,
            set_permission_mode,
            set_session_mode,
            set_session_model,
            get_session_usage,
            get_session_recap,
            get_rewind_points,
            rewind_execute,
            cancel_subagent,
            list_subagents,
            kill_task,
            list_tasks,
            set_task_permission_mode,
            get_task_permission_mode,
            list_task_permission_modes,
            get_last_spawn_permission_mode,
            set_task_plan_armed,
            get_task_plan_armed,
            list_task_plan_armed,
            list_project_dir,
            open_project_path,
            read_project_file,
            git_status,
            git_branch_info,
            git_diff_file,
            git_stage_file,
            git_unstage_file,
            git_stage_all,
            git_unstage_all,
            git_commit,
            git_apply_patch,
        ])
        .run(context)
        .expect("error while running tauri application");
}
