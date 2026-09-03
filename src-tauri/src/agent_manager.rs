use crate::acp::protocol::SessionModelsInfo;
use crate::acp::{AcpClient, NotifyFn};
use crate::agent_fs::write_text_file;
use crate::agent_runtime::{find_grok_bin, now_iso, truncate_text};
use crate::agent_types::{
    AttachRequest, ManagedAgentInfo, ManagedStatus, PendingPermission, PermissionKind,
    PermissionMode, ResolvePermissionRequest, SpawnRequest,
};
use crate::permission_policy::{
    decide_gate, is_allow_option, is_enable_always_approve, pick_allow_option, pick_reject_option,
    GateDecision,
};
use crate::rpc_handler::{self, HandleResult, ResponseAction};
use crate::sessions;
use crate::shell_emitter;
use crate::shell_stream::ShellStream;
use crate::task_prefs;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

mod models;
mod reconnect;

struct LiveAgent {
    info: ManagedAgentInfo,
    client: Arc<AcpClient>,
    connection_generation: u64,
    reconnecting: bool,
    grok_bin: String,
    global_args: Vec<String>,
    agent_args: Vec<String>,
    in_flight_prompts: HashSet<String>,
    current_prompt_id: Option<String>,
    last_activate: Option<Instant>,
    reconnect_burst: u32,
}

struct RequestTarget {
    client: Arc<AcpClient>,
    permission_mode: PermissionMode,
    session_hint: Option<String>,
}

#[derive(Clone)]
struct PendingGate {
    permission: PendingPermission,
    client: Arc<AcpClient>,
}

const INITIAL_CONNECTION_GENERATION: u64 = 1;
const RECONNECT_UNSTABLE: Duration = Duration::from_secs(5);
const RECONNECT_BURST_LIMIT: u32 = 3;

/// Count deaths inside the last-activate window. A stale or missing activate
/// resets the burst so a later transport loss can reconnect again.
pub(crate) fn next_reconnect_burst(
    last_activate: Option<Instant>,
    now: Instant,
    burst: u32,
) -> u32 {
    let unstable = last_activate
        .map(|t| now.saturating_duration_since(t) < RECONNECT_UNSTABLE)
        .unwrap_or(false);
    if unstable {
        burst.saturating_add(1)
    } else {
        0
    }
}

pub(crate) fn can_reconnect_after_burst(has_session: bool, burst: u32) -> bool {
    has_session && burst < RECONNECT_BURST_LIMIT
}

fn finish_prompt(
    in_flight: &mut HashSet<String>,
    current_prompt_id: &mut Option<String>,
    prompt_id: &str,
) -> bool {
    in_flight.remove(prompt_id);
    let is_current = current_prompt_id.as_deref() == Some(prompt_id);
    if is_current {
        *current_prompt_id = None;
    }
    is_current
}

fn finish_prompt_status(
    in_flight: &mut HashSet<String>,
    current_prompt_id: &mut Option<String>,
    pending_permission_count: u32,
    prompt_id: &str,
) -> Option<ManagedStatus> {
    if !finish_prompt(in_flight, current_prompt_id, prompt_id) {
        return None;
    }
    Some(if pending_permission_count > 0 {
        ManagedStatus::AwaitingPermission
    } else if in_flight.is_empty() {
        ManagedStatus::Ready
    } else {
        ManagedStatus::Running
    })
}

fn pending_count(inner: &Inner, handle_id: &str) -> u32 {
    inner
        .pending
        .lock()
        .values()
        .filter(|entry| entry.permission.handle_id == handle_id)
        .count() as u32
}

fn take_pending_for_handle(inner: &Inner, handle_id: &str) -> Vec<PendingGate> {
    let mut pending = inner.pending.lock();
    let keys: Vec<String> = pending
        .iter()
        .filter(|(_, entry)| entry.permission.handle_id == handle_id)
        .map(|(key, _)| key.clone())
        .collect();
    keys.into_iter()
        .filter_map(|key| pending.remove(&key))
        .collect()
}

fn is_active_generation(inner: &Inner, handle_id: &str, generation: u64) -> bool {
    inner
        .agents
        .lock()
        .get(handle_id)
        .is_some_and(|agent| agent.connection_generation == generation && !agent.reconnecting)
}

fn terminal_prompt_id<'a>(method: &str, params: &'a Value) -> Option<Option<&'a str>> {
    if method == "x.ai/session/prompt_complete" {
        return Some(
            params
                .get("promptId")
                .or_else(|| params.get("prompt_id"))
                .and_then(Value::as_str),
        );
    }
    if method != "session/update" && !method.ends_with("/session/update") {
        return None;
    }
    let update = params.get("update").unwrap_or(params);
    if update.get("sessionUpdate").and_then(Value::as_str) != Some("turn_completed") {
        return None;
    }
    Some(
        update
            .get("prompt_id")
            .or_else(|| update.get("promptId"))
            .and_then(Value::as_str)
            .or_else(|| {
                params
                    .pointer("/_meta/promptId")
                    .or_else(|| params.pointer("/_meta/prompt_id"))
                    .and_then(Value::as_str)
            }),
    )
}

struct AttachReservation {
    inner: Arc<Inner>,
    session_id: String,
}

impl Drop for AttachReservation {
    fn drop(&mut self) {
        self.inner
            .attaching_sessions
            .lock()
            .remove(&self.session_id);
    }
}

pub(crate) struct Inner {
    pub(crate) app: Mutex<Option<AppHandle>>,
    pub(crate) shell_stream: ShellStream,
    agents: Mutex<HashMap<String, LiveAgent>>,
    pending: Mutex<HashMap<String, PendingGate>>,
    early_requests: Mutex<Vec<(String, Value)>>,
    attaching_sessions: Mutex<HashSet<String>>,
    grok_bin: Mutex<Option<String>>,
    handlers: Vec<Box<dyn rpc_handler::RpcHandler>>,
}

#[derive(Clone)]
pub struct AgentManager {
    inner: Arc<Inner>,
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                app: Mutex::new(None),
                agents: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                early_requests: Mutex::new(Vec::new()),
                attaching_sessions: Mutex::new(HashSet::new()),
                grok_bin: Mutex::new(None),
                shell_stream: ShellStream::default(),
                handlers: rpc_handler::default_handlers(),
            }),
        }
    }

    pub fn set_session_mode(&self, handle_id: &str, mode_id: &str) -> Result<(), String> {
        let mode_id = mode_id.trim();
        if mode_id.is_empty() {
            return Err("mode_id is empty".into());
        }
        let (session_id, client) = self.resolve_target(handle_id)?;
        client
            .session_set_mode(&session_id, mode_id)
            .map_err(|error| error.user_message())?;
        Ok(())
    }

    /// ACP `session/set_model` — switch the model for a session.
    pub fn set_session_model(
        &self,
        handle_id: &str,
        model_id: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<Value, String> {
        let model_id = model_id.trim();
        if model_id.is_empty() {
            return Err("model_id is empty".into());
        }
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .set_session_model(&session_id, model_id, reasoning_effort)
            .map_err(|error| error.user_message())?;
        // Reflect the switch immediately so the UI dropdown does not lag on
        // a later models/update or session snapshot.
        let updated = {
            let mut agents = self.inner.agents.lock();
            agents.get_mut(handle_id).map(|agent| {
                agent.info.model_id = Some(model_id.to_string());
                agent.info.reasoning_effort = reasoning_effort
                    .map(str::trim)
                    .filter(|effort| !effort.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        models::advertised_effort_for(&agent.info.available_models, Some(model_id))
                    });
                agent.info.clone()
            })
        };
        if let Some(info) = updated {
            Self::emit_status(&self.inner, &info);
        }
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    /// ACP `x.ai/session/usage` — cumulative session token usage.
    pub fn session_usage(&self, handle_id: &str) -> Result<Value, String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .session_usage(&session_id)
            .map_err(|error| error.user_message())?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    /// ACP `x.ai/recap` — request recap (text arrives via session_recap notification).
    pub fn recap(&self, handle_id: &str, auto: bool) -> Result<Value, String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .recap(&session_id, auto)
            .map_err(|error| error.user_message())?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    /// ACP `x.ai/rewind/points` — list rewindable points.
    pub fn rewind_points(&self, handle_id: &str) -> Result<Value, String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .rewind_points(&session_id)
            .map_err(|error| error.user_message())?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    /// ACP `x.ai/rewind/execute` — rewind to a target prompt index.
    pub fn rewind_execute(
        &self,
        handle_id: &str,
        target_prompt_index: u64,
        mode: Option<&str>,
    ) -> Result<Value, String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .rewind_execute(&session_id, target_prompt_index, mode)
            .map_err(|error| error.user_message())?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    /// ACP `x.ai/subagent/cancel` — cancel a running subagent.
    pub fn cancel_subagent(&self, handle_id: &str, subagent_id: &str) -> Result<Value, String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .cancel_subagent(&session_id, subagent_id)
            .map_err(|error| error.user_message())?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    /// ACP `x.ai/subagent/list_running` — list running subagents.
    pub fn list_subagents(&self, handle_id: &str) -> Result<Value, String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .list_subagents(&session_id)
            .map_err(|error| error.user_message())?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    /// ACP `x.ai/task/kill` — kill a background task.
    pub fn kill_task(&self, handle_id: &str, task_id: &str) -> Result<Value, String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .kill_task(&session_id, task_id)
            .map_err(|error| error.user_message())?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    /// ACP `x.ai/task/list` — list background tasks.
    pub fn list_tasks(&self, handle_id: &str) -> Result<Value, String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        let result = client
            .list_tasks(&session_id)
            .map_err(|error| error.user_message())?;
        Ok(serde_json::to_value(result).unwrap_or_default())
    }

    pub fn interject(&self, handle_id: &str, text: &str) -> Result<Value, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("interjection is empty".into());
        }
        let (session_id, client) = self.resolve_target(handle_id)?;
        client
            .session_interject(&session_id, text)
            .map_err(|error| error.user_message())
    }

    /// Resolve a handle_id into its (session_id, AcpClient) pair.
    fn resolve_target(&self, handle_id: &str) -> Result<(String, Arc<AcpClient>), String> {
        let agents = self.inner.agents.lock();
        let agent = agents
            .get(handle_id)
            .ok_or_else(|| format!("unknown handle {handle_id}"))?;
        let session_id = agent
            .info
            .session_id
            .clone()
            .ok_or_else(|| "agent has no session_id".to_string())?;
        Ok((session_id, Arc::clone(&agent.client)))
    }

    pub fn queue_remove(&self, handle_id: &str, id: &str, version: u64) -> Result<(), String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        client
            .queue_remove(&session_id, id, version)
            .map_err(|error| error.user_message())
    }

    pub fn queue_reorder(&self, handle_id: &str, ordered_ids: &[String]) -> Result<(), String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        client
            .queue_reorder(&session_id, ordered_ids)
            .map_err(|error| error.user_message())
    }

    pub fn queue_clear(&self, handle_id: &str) -> Result<(), String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        client
            .queue_clear(&session_id)
            .map_err(|error| error.user_message())
    }

    pub fn queue_edit(&self, handle_id: &str, id: &str, new_text: &str) -> Result<(), String> {
        if new_text.trim().is_empty() {
            return Err("queued prompt is empty".into());
        }
        let (session_id, client) = self.resolve_target(handle_id)?;
        client
            .queue_edit(&session_id, id, new_text)
            .map_err(|error| error.user_message())
    }

    pub fn queue_interject(&self, handle_id: &str, id: &str, version: u64) -> Result<(), String> {
        let (session_id, client) = self.resolve_target(handle_id)?;
        client
            .queue_interject(&session_id, id, version)
            .map_err(|error| error.user_message())
    }

    pub fn set_permission_mode(
        &self,
        handle_id: &str,
        mode: PermissionMode,
    ) -> Result<ManagedAgentInfo, String> {
        let (session_id, client, changed, previous_mode) = {
            let mut agents = self.inner.agents.lock();
            let agent = agents
                .get_mut(handle_id)
                .ok_or_else(|| format!("unknown handle {handle_id}"))?;
            let previous_mode = agent.info.permission_mode;
            let changed = previous_mode != mode;
            agent.info.permission_mode = mode;
            agent.info.always_approve = mode.spawns_always_approve();
            Self::emit_status(&self.inner, &agent.info);
            let session_id = agent.info.session_id.clone();
            let client = Arc::clone(&agent.client);
            (session_id, client, changed, previous_mode)
        };
        if let Some(sid) = session_id.as_deref() {
            if let Err(error) = task_prefs::set_permission_mode(sid, mode) {
                if let Some(agent) = self.inner.agents.lock().get_mut(handle_id) {
                    agent.info.permission_mode = previous_mode;
                    agent.info.always_approve = previous_mode.spawns_always_approve();
                    Self::emit_status(&self.inner, &agent.info);
                }
                return Err(error);
            }
        }
        if changed {
            Self::notify_mode_changed(&client, mode);
        }
        self.reconcile_pending_for_mode(handle_id, mode);
        self.get(handle_id)
            .ok_or_else(|| format!("unknown handle {handle_id}"))
    }

    /// Send x.ai/yolo_mode_changed notification to shell so its permission
    /// manager stays in sync with the host-side mode.
    fn notify_mode_changed(client: &AcpClient, mode: PermissionMode) {
        let permission_mode = match mode {
            PermissionMode::BypassPermissions => "always-approve",
            PermissionMode::Auto => "auto",
            _ => "ask",
        };
        if let Err(e) = client.notify_yolo_mode(
            mode == PermissionMode::BypassPermissions,
            mode == PermissionMode::Auto,
            permission_mode,
        ) {
            tracing::warn!(error = %e, "failed to send yolo_mode_changed notification");
        }
    }

    fn reconcile_pending_for_mode(&self, handle_id: &str, mode: PermissionMode) {
        let items: Vec<(String, String)> = {
            let pending = self.inner.pending.lock();
            pending
                .values()
                .map(|entry| &entry.permission)
                .filter(|permission| permission.handle_id == handle_id)
                .filter_map(|permission| {
                    let decision = decide_gate(mode, permission);
                    match decision {
                        GateDecision::Ask => None,
                        GateDecision::Allow => Some((
                            permission.request_key.clone(),
                            pick_allow_option(&permission.options),
                        )),
                        GateDecision::Deny => Some((
                            permission.request_key.clone(),
                            pick_reject_option(&permission.options, "reject-once"),
                        )),
                    }
                })
                .collect()
        };
        for (key, option_id) in items {
            let _ = self.resolve_permission(ResolvePermissionRequest {
                handle_id: handle_id.to_string(),
                request_key: key,
                option_id,
                comments: None,
                payload: None,
            });
        }
    }

    fn fail_registered_agent(
        inner: &Arc<Inner>,
        handle_id: &str,
        info: &mut ManagedAgentInfo,
        err: String,
    ) -> String {
        drop(take_pending_for_handle(inner, handle_id));

        // Publish the terminal state before killing the child so its expected
        // stdout EOF cannot race the transport-close handler into reconnecting
        // a bootstrap that already failed.
        info.status = ManagedStatus::Error;
        info.last_error = Some(err.clone());
        info.pid = None;
        info.pending_permission_count = 0;
        let client = {
            let mut agents = inner.agents.lock();
            agents.get_mut(handle_id).map(|agent| {
                agent.reconnecting = false;
                agent.info = info.clone();
                Arc::clone(&agent.client)
            })
        };
        if let Some(client) = client {
            let _ = client.kill();
        }

        Self::emit_status(inner, info);
        err
    }

    pub fn set_app(&self, app: AppHandle) {
        *self.inner.app.lock() = Some(app);
    }

    pub(crate) fn emit(inner: &Inner, event: &str, payload: Value) {
        if let Some(app) = inner.app.lock().as_ref() {
            let _ = app.emit(event, payload);
        }
    }

    pub(crate) fn emit_status(inner: &Inner, info: &ManagedAgentInfo) {
        if let Ok(v) = serde_json::to_value(info) {
            Self::emit(inner, "agent-status", v);
        }
    }

    fn send_action(client: &AcpClient, id: &Value, action: ResponseAction) -> Result<(), String> {
        match action {
            ResponseAction::Send(value) => {
                client.respond_result(id, value).map_err(|e| e.to_string())
            }
            ResponseAction::SendError(code, msg) => client
                .respond_error(id, code, &msg)
                .map_err(|e| e.to_string()),
            ResponseAction::WriteFile { path, content } => {
                write_text_file(&path, &content).map_err(|e| e.to_string())?;
                client
                    .respond_result(id, Value::Null)
                    .map_err(|e| e.to_string())
            }
        }
    }

    pub fn resolve_grok_bin(&self) -> Result<String, String> {
        if let Some(cached) = self.inner.grok_bin.lock().clone() {
            if Path::new(&cached).is_file() {
                return Ok(cached);
            }
            tracing::warn!(
                path = cached,
                "cached Grok binary disappeared; falling back to discovery"
            );
            *self.inner.grok_bin.lock() = None;
        }
        let bin = find_grok_bin().ok_or_else(|| {
            "Could not find `grok` binary. Set GROK_BIN or install Grok Build.".to_string()
        })?;
        *self.inner.grok_bin.lock() = Some(bin.clone());
        Ok(bin)
    }

    pub fn list(&self) -> Vec<ManagedAgentInfo> {
        self.inner
            .agents
            .lock()
            .values()
            .map(|a| a.info.clone())
            .collect()
    }

    pub fn get(&self, handle_id: &str) -> Option<ManagedAgentInfo> {
        self.inner
            .agents
            .lock()
            .get(handle_id)
            .map(|a| a.info.clone())
    }

    pub fn find_by_session(&self, session_id: &str) -> Option<ManagedAgentInfo> {
        self.inner
            .agents
            .lock()
            .values()
            .find(|a| a.info.session_id.as_deref() == Some(session_id))
            .map(|a| a.info.clone())
    }

    pub fn list_pending_permissions(&self, handle_id: Option<String>) -> Vec<PendingPermission> {
        let mut list: Vec<_> = self
            .inner
            .pending
            .lock()
            .values()
            .map(|entry| &entry.permission)
            .filter(|permission| {
                handle_id
                    .as_ref()
                    .map(|h| h == &permission.handle_id)
                    .unwrap_or(true)
            })
            .cloned()
            .collect();
        list.sort_by_key(|p| p.created_at_ms);
        list
    }

    fn make_notify(inner: &Arc<Inner>, handle_id: &str, generation: u64) -> NotifyFn {
        let inner = Arc::clone(inner);
        let handle_id = handle_id.to_string();
        Arc::new(move |msg: Value| {
            let method = msg
                .get("method")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();

            if method == "_pinkcode/transport_closed" {
                let reason = msg
                    .pointer("/params/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("ACP transport closed");
                let failure = msg
                    .pointer("/params/failure")
                    .and_then(Value::as_str)
                    .unwrap_or("recv_failed");
                Self::handle_transport_closed(&inner, &handle_id, generation, reason, failure);
                return;
            }

            Self::route_agent_message(&inner, &handle_id, generation, msg);
        })
    }

    fn route_agent_message(inner: &Arc<Inner>, handle_id: &str, generation: u64, msg: Value) {
        // A replaced transport can still flush buffered messages. Keep every
        // state transition and UI event scoped to the transport that owns the
        // handle now.
        if !is_active_generation(inner, handle_id, generation) {
            return;
        }

        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let has_id = msg.get("id").is_some();
        let is_response_shape = msg.get("result").is_some() || msg.get("error").is_some();

        if has_id && !method.is_empty() && !is_response_shape {
            Self::handle_server_request(inner, handle_id, &msg);
            return;
        }

        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let session_id = params
            .get("sessionId")
            .or_else(|| params.get("session_id"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        let event_id = msg
            .pointer("/_meta/eventId")
            .or_else(|| msg.pointer("/_meta/event_id"))
            .or_else(|| params.pointer("/_meta/eventId"))
            .and_then(Value::as_str);

        if let Some(prompt_id) = terminal_prompt_id(&method, &params) {
            Self::finish_notified_prompt(inner, handle_id, generation, prompt_id);
        }

        if method == "x.ai/queue/changed" {
            if let Some(prompt_id) = params.get("runningPromptId").and_then(Value::as_str) {
                if reconnect::should_adopt_prompt_id(prompt_id) {
                    if let Some(agent) = inner.agents.lock().get_mut(handle_id) {
                        agent.current_prompt_id = Some(prompt_id.to_string());
                    }
                }
            }
        }

        // Non-blocking agent startup: catalog may arrive after ready via
        // `x.ai/models/update` (or `_x.ai/models/update` on some transports).
        if models::is_models_update_method(&method) {
            Self::apply_models_update_notification(inner, handle_id, &params);
        }

        let payload = json!({
            "handleId": handle_id,
            "sessionId": session_id,
            "method": method,
            "eventId": event_id,
            "params": params,
        });

        if method == "session/update" || method.ends_with("/session/update") {
            Self::emit(inner, "agent-update", payload.clone());
            shell_emitter::maybe_emit_shell(
                &inner.shell_stream,
                inner,
                handle_id,
                &session_id,
                &params,
            );
        } else if !method.is_empty() {
            Self::emit(inner, "agent-notification", payload);
        }
    }

    fn apply_models_update_notification(inner: &Arc<Inner>, handle_id: &str, params: &Value) {
        let models: SessionModelsInfo = match serde_json::from_value(params.clone()) {
            Ok(m) => m,
            Err(error) => {
                tracing::debug!(
                    handle_id,
                    error = %error,
                    "ignore unreadable x.ai/models/update params"
                );
                return;
            }
        };
        let updated = {
            let mut agents = inner.agents.lock();
            let Some(agent) = agents.get_mut(handle_id) else {
                return;
            };
            models::apply_catalog_refresh(&mut agent.info, &models);
            agent.info.clone()
        };
        Self::emit_status(inner, &updated);
    }

    fn finish_notified_prompt(
        inner: &Arc<Inner>,
        handle_id: &str,
        generation: u64,
        prompt_id: Option<&str>,
    ) {
        let updated = {
            let mut agents = inner.agents.lock();
            let Some(agent) = agents.get_mut(handle_id) else {
                return;
            };
            if agent.connection_generation != generation || agent.reconnecting {
                return;
            }
            let completed_prompt_id = prompt_id
                .map(str::to_string)
                .or_else(|| agent.current_prompt_id.clone());
            let Some(completed_prompt_id) = completed_prompt_id else {
                return;
            };
            let Some(status) = finish_prompt_status(
                &mut agent.in_flight_prompts,
                &mut agent.current_prompt_id,
                agent.info.pending_permission_count,
                &completed_prompt_id,
            ) else {
                return;
            };
            agent.info.status = status;
            agent.info.clone()
        };
        Self::emit_status(inner, &updated);
    }

    fn dispatch_handle_request(
        inner: &Arc<Inner>,
        handle_id: &str,
        request_id: Value,
        method: &str,
        params: Value,
    ) {
        let (client, permission_mode, session_hint) = {
            let agents = inner.agents.lock();
            match agents.get(handle_id) {
                Some(a) => (
                    Arc::clone(&a.client),
                    a.info.permission_mode,
                    a.info.session_id.clone(),
                ),
                None => {
                    inner.early_requests.lock().push((
                        handle_id.to_string(),
                        json!({
                            "id": request_id,
                            "method": method,
                            "params": params,
                        }),
                    ));
                    return;
                }
            }
        };

        Self::dispatch_handle_request_to(
            inner,
            handle_id,
            request_id,
            method,
            params,
            RequestTarget {
                client,
                permission_mode,
                session_hint,
            },
        );
    }

    fn dispatch_handle_request_to(
        inner: &Arc<Inner>,
        handle_id: &str,
        request_id: Value,
        method: &str,
        params: Value,
        target: RequestTarget,
    ) {
        let session_id = params
            .get("sessionId")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .or(target.session_hint);

        let handler = rpc_handler::find_handler(&inner.handlers, method);
        match handler {
            Some(h) => match h.handle_request(handle_id, session_id, request_id.clone(), &params) {
                HandleResult::Respond(value) => {
                    let _ = target.client.respond_result(&request_id, value);
                }
                HandleResult::Gate(pending) => {
                    match decide_gate(target.permission_mode, &pending) {
                        GateDecision::Allow => {
                            let action = rpc_handler::build_allow_response(h, &pending);
                            match action {
                                Ok(a) => {
                                    let _ =
                                        Self::send_action(&target.client, &pending.request_id, a);
                                }
                                Err(e) => {
                                    let _ = target.client.respond_error(&request_id, -32000, &e);
                                }
                            }
                        }
                        GateDecision::Deny => {
                            let action = rpc_handler::build_deny_response(h, &pending);
                            match action {
                                Ok(a) => {
                                    let _ =
                                        Self::send_action(&target.client, &pending.request_id, a);
                                }
                                Err(e) => {
                                    let _ = target.client.respond_error(&request_id, -32000, &e);
                                }
                            }
                        }
                        GateDecision::Ask => {
                            Self::enqueue_permission(
                                inner,
                                handle_id,
                                *pending,
                                Arc::clone(&target.client),
                            );
                        }
                    }
                }
                HandleResult::Error(code, msg) => {
                    let _ = target.client.respond_error(&request_id, code, &msg);
                }
            },
            None => {
                let _ = target.client.respond_error(
                    &request_id,
                    -32601,
                    &format!("PinkCode does not implement {method}"),
                );
            }
        }
    }

    fn handle_transport_closed(
        inner: &Arc<Inner>,
        handle_id: &str,
        generation: u64,
        reason: &str,
        failure: &str,
    ) {
        let is_current = inner
            .agents
            .lock()
            .get(handle_id)
            .map(|agent| {
                agent.connection_generation == generation
                    && !agent.reconnecting
                    && !matches!(
                        agent.info.status,
                        ManagedStatus::Stopping | ManagedStatus::Stopped | ManagedStatus::Error
                    )
            })
            .unwrap_or(false);
        if !is_current {
            return;
        }

        inner.shell_stream.clear_handle(handle_id);
        for entry in take_pending_for_handle(inner, handle_id) {
            if let Ok(value) = serde_json::to_value(&entry.permission) {
                Self::emit(
                    inner,
                    "agent-permission-resolved",
                    json!({
                        "pending": value,
                        "optionId": "transport-closed",
                        "allowed": false,
                    }),
                );
            }
        }

        let (updated, should_reconnect, kill_client) = {
            let mut agents = inner.agents.lock();
            match agents.get_mut(handle_id) {
                Some(agent) => {
                    if agent.connection_generation != generation || agent.reconnecting {
                        return;
                    }
                    if matches!(
                        agent.info.status,
                        ManagedStatus::Stopping | ManagedStatus::Stopped | ManagedStatus::Error
                    ) {
                        return;
                    }
                    agent.reconnect_burst = next_reconnect_burst(
                        agent.last_activate,
                        Instant::now(),
                        agent.reconnect_burst,
                    );
                    let can_reconnect = can_reconnect_after_burst(
                        agent.info.session_id.is_some(),
                        agent.reconnect_burst,
                    );
                    agent.info.status = if can_reconnect {
                        ManagedStatus::Starting
                    } else {
                        ManagedStatus::Error
                    };
                    agent.reconnecting = can_reconnect;
                    agent.info.pid = None;
                    agent.info.pending_permission_count = 0;
                    agent.info.last_error = Some(if can_reconnect {
                        format!("ACP {failure}; reconnecting after transport loss: {reason}")
                    } else if agent.reconnect_burst >= 3 {
                        format!("ACP {failure}: {reason} (gave up after repeated reconnects)")
                    } else {
                        format!("ACP {failure}: {reason}")
                    });
                    let kill_on_give_up = if can_reconnect {
                        None
                    } else {
                        Some(Arc::clone(&agent.client))
                    };
                    (Some(agent.info.clone()), can_reconnect, kill_on_give_up)
                }
                None => (None, false, None),
            }
        };
        if let Some(info) = updated {
            Self::emit_status(inner, &info);
        }
        if should_reconnect {
            Self::spawn_reconnect(Arc::clone(inner), handle_id.to_string(), generation);
        } else if let Some(client) = kill_client {
            // Giving up without a kill leaves grok in active_sessions.json,
            // which the UI then labels "Open in Grok Build".
            let _ = client.kill();
        }
    }

    fn spawn_reconnect(inner: Arc<Inner>, handle_id: String, failed_generation: u64) {
        reconnect::spawn(inner, handle_id, failed_generation);
    }

    fn handle_server_request(inner: &Arc<Inner>, handle_id: &str, msg: &Value) {
        let method = msg
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let request_id = msg.get("id").cloned().unwrap_or(Value::Null);
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        Self::dispatch_handle_request(inner, handle_id, request_id, &method, params);
    }

    fn enqueue_permission(
        inner: &Arc<Inner>,
        handle_id: &str,
        pending: PendingPermission,
        client: Arc<AcpClient>,
    ) {
        let key = pending.request_key.clone();
        if let Ok(v) = serde_json::to_value(&pending) {
            Self::emit(inner, "agent-permission", v);
        }
        let mut agents = inner.agents.lock();
        let count = {
            let mut map = inner.pending.lock();
            map.insert(
                key,
                PendingGate {
                    permission: pending,
                    client,
                },
            );
            map.values()
                .filter(|entry| entry.permission.handle_id == handle_id)
                .count() as u32
        };
        if let Some(a) = agents.get_mut(handle_id) {
            a.info.pending_permission_count = count;
            a.info.status = ManagedStatus::AwaitingPermission;
            Self::emit_status(inner, &a.info);
        }
    }

    fn drain_early_requests(inner: &Arc<Inner>, handle_id: &str) {
        let early: Vec<Value> = {
            let mut q = inner.early_requests.lock();
            let mut take = Vec::new();
            q.retain(|(h, msg)| {
                if h == handle_id {
                    take.push(msg.clone());
                    false
                } else {
                    true
                }
            });
            take
        };
        for msg in early {
            Self::handle_server_request(inner, handle_id, &msg);
        }
    }

    fn start_client(
        &self,
        mut info: ManagedAgentInfo,
        global_args: &[String],
        agent_args: &[String],
    ) -> Result<(ManagedAgentInfo, Arc<AcpClient>), String> {
        let handle_id = info.handle_id.clone();
        Self::emit_status(&self.inner, &info);
        let generation = INITIAL_CONNECTION_GENERATION;
        let notify = Self::make_notify(&self.inner, &handle_id, generation);
        let grok_bin = self.resolve_grok_bin()?;
        let client = Arc::new(
            AcpClient::spawn_with_notify(
                &grok_bin,
                info.always_approve,
                global_args,
                agent_args,
                notify,
            )
            .map_err(|error| error.to_string())?,
        );
        info.pid = Some(client.pid());
        self.inner.agents.lock().insert(
            handle_id.clone(),
            LiveAgent {
                info: info.clone(),
                client: Arc::clone(&client),
                connection_generation: generation,
                reconnecting: false,
                grok_bin,
                global_args: global_args.to_vec(),
                agent_args: agent_args.to_vec(),
                in_flight_prompts: HashSet::new(),
                current_prompt_id: None,
                last_activate: None,
                reconnect_burst: 0,
            },
        );
        Self::drain_early_requests(&self.inner, &handle_id);
        let initialized = match client.initialize() {
            Ok(value) => value,
            Err(error) => {
                return Err(Self::fail_registered_agent(
                    &self.inner,
                    &handle_id,
                    &mut info,
                    error.user_message(),
                ));
            }
        };
        if let Err(error) = client.authenticate_if_available(&initialized) {
            tracing::warn!(error = %error, "ACP eager authentication failed; continuing");
        }
        Ok((info, client))
    }

    pub fn resolve_permission(
        &self,
        req: ResolvePermissionRequest,
    ) -> Result<PendingPermission, String> {
        let entry = self
            .inner
            .pending
            .lock()
            .remove(&req.request_key)
            .ok_or_else(|| format!("unknown permission request {}", req.request_key))?;

        if entry.permission.handle_id != req.handle_id {
            self.inner
                .pending
                .lock()
                .insert(req.request_key.clone(), entry);
            return Err("handle_id mismatch".into());
        }
        let PendingGate {
            permission: pending,
            client,
        } = entry;

        let allow = match rpc_handler::find_handler_by_kind(&self.inner.handlers, pending.kind) {
            Some(handler) => handler.is_allow_option(&req.option_id),
            None => is_allow_option(&req.option_id, &pending.options),
        };

        let delivery = match rpc_handler::find_handler_by_kind(&self.inner.handlers, pending.kind) {
            Some(handler) => {
                match handler.build_response(
                    &pending,
                    &req.option_id,
                    allow,
                    req.comments.as_deref(),
                    req.payload.as_ref(),
                ) {
                    Ok(action) => Self::send_action(&client, &pending.request_id, action),
                    Err(msg) => client
                        .respond_error(&pending.request_id, -32000, &msg)
                        .map_err(|e| e.to_string()),
                }
            }
            None => {
                if allow {
                    client
                        .respond_result(&pending.request_id, Value::Null)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .respond_error(
                            &pending.request_id,
                            -32000,
                            "User denied request in PinkCode",
                        )
                        .map_err(|e| e.to_string())
                }
            }
        };

        if let Err(error) = delivery {
            let agent_can_retry = self
                .inner
                .agents
                .lock()
                .get(&req.handle_id)
                .map(|agent| {
                    !agent.reconnecting
                        && Arc::ptr_eq(&agent.client, &client)
                        && !matches!(
                            agent.info.status,
                            ManagedStatus::Stopped | ManagedStatus::Error
                        )
                })
                .unwrap_or(false);
            if agent_can_retry {
                self.inner.pending.lock().insert(
                    req.request_key.clone(),
                    PendingGate {
                        permission: pending,
                        client: Arc::clone(&client),
                    },
                );
            } else if let Ok(value) = serde_json::to_value(&pending) {
                Self::emit(
                    &self.inner,
                    "agent-permission-resolved",
                    json!({
                        "pending": value,
                        "optionId": "transport-closed",
                        "allowed": false,
                    }),
                );
            }
            return Err(format!("failed to deliver permission response: {error}"));
        }

        // enable-always-approve: after sending the response, activate YOLO mode.
        if is_enable_always_approve(&req.option_id) {
            if let Err(e) =
                self.set_permission_mode(&req.handle_id, PermissionMode::BypassPermissions)
            {
                tracing::warn!(error = %e, "failed to activate YOLO mode");
            }
        }

        {
            let mut agents = self.inner.agents.lock();
            let count = pending_count(&self.inner, &req.handle_id);
            if let Some(a) = agents.get_mut(&req.handle_id) {
                a.info.pending_permission_count = count;
                if count == 0 && a.info.status == ManagedStatus::AwaitingPermission {
                    a.info.status = ManagedStatus::Running;
                }
                Self::emit_status(&self.inner, &a.info);
            }
        }

        if let Ok(v) = serde_json::to_value(&pending) {
            Self::emit(
                &self.inner,
                "agent-permission-resolved",
                json!({
                    "pending": v,
                    "optionId": req.option_id,
                    "allowed": allow,
                }),
            );
        }

        Ok(pending)
    }

    pub fn spawn(&self, req: SpawnRequest) -> Result<ManagedAgentInfo, String> {
        let cwd = req.cwd.trim().to_string();
        if cwd.is_empty() || !Path::new(&cwd).is_dir() {
            return Err(format!("Invalid working directory: {cwd}"));
        }
        let permission_mode = PermissionMode::from_request(req.permission_mode, req.always_approve);
        let always_approve = permission_mode.spawns_always_approve();
        task_prefs::set_last_spawn_mode(permission_mode)?;
        let handle_id = Uuid::new_v4().to_string();

        // Top-level flags (before `agent`) vs agent-subcommand flags (after).
        let global_args = permission_mode.spawn_global_args();
        let mut agent_args = permission_mode.spawn_agent_args();
        if let Some(model) = &req.model {
            if !model.is_empty() {
                agent_args.push("-m".into());
                agent_args.push(model.clone());
            }
        }

        let info = ManagedAgentInfo {
            handle_id: handle_id.clone(),
            session_id: None,
            cwd: cwd.clone(),
            pid: None,
            status: ManagedStatus::Starting,
            permission_mode,
            always_approve,
            model_id: req.model.clone(),
            reasoning_effort: None,
            available_models: Vec::new(),
            last_error: None,
            title: req
                .prompt
                .as_ref()
                .map(|p| truncate_text(p, 80))
                .or_else(|| Some("New agent".into())),
            created_at: now_iso(),
            pending_permission_count: 0,
        };
        let (mut info, client) = self.start_client(info, &global_args, &agent_args)?;

        let result = match client.session_new(&cwd) {
            Ok(r) => r,
            Err(e) => {
                return Err(Self::fail_registered_agent(
                    &self.inner,
                    &handle_id,
                    &mut info,
                    e.user_message(),
                ));
            }
        };

        let session_id = match result.resolve_session_id(None) {
            Some(id) => id,
            None => {
                return Err(Self::fail_registered_agent(
                    &self.inner,
                    &handle_id,
                    &mut info,
                    "session/new missing sessionId".into(),
                ));
            }
        };

        info.session_id = Some(session_id.clone());
        if let Some(a) = self.inner.agents.lock().get_mut(&handle_id) {
            a.last_activate = Some(Instant::now());
        }
        task_prefs::set_permission_mode(&session_id, permission_mode)?;
        if let Some(models) = result.models.as_ref() {
            models::apply_models_info(&mut info, models);
        }
        info.status = ManagedStatus::Ready;
        if let Some(a) = self.inner.agents.lock().get_mut(&handle_id) {
            a.info = info.clone();
        }
        Self::emit_status(&self.inner, &info);

        if let Some(mode_id) = req
            .session_mode_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if let Err(e) = client.session_set_mode(&session_id, mode_id) {
                tracing::warn!(mode_id, error = %e, "session/set_mode after spawn failed");
            }
        }

        if let Some(prompt) = req.prompt.filter(|p| !p.trim().is_empty()) {
            self.dispatch_prompt(
                &handle_id,
                &session_id,
                prompt,
                client,
                INITIAL_CONNECTION_GENERATION,
            );
            // dispatch_prompt flips status to Running and emits; return the live
            // snapshot so the spawn invoke does not clobber UI back to Ready.
            if let Some(live) = self.get(&handle_id) {
                return Ok(live);
            }
        }

        Ok(info)
    }

    pub fn attach(&self, req: AttachRequest) -> Result<ManagedAgentInfo, sessions::CommandError> {
        let cwd = req.cwd.trim().to_string();
        let session_id = req.session_id.trim().to_string();
        if session_id.is_empty() {
            return Err("session_id required".into());
        }
        if cwd.is_empty() || !Path::new(&cwd).is_dir() {
            return Err(format!("Invalid working directory: {cwd}").into());
        }

        let existing = {
            let agents = self.inner.agents.lock();
            agents
                .values()
                .find(|a| {
                    a.info.session_id.as_deref() == Some(session_id.as_str())
                        && !matches!(a.info.status, ManagedStatus::Stopped | ManagedStatus::Error)
                })
                .map(|a| (a.info.clone(), a.info.pid))
        };
        if let Some((info, pid)) = existing {
            if let Some(msg) = sessions::session_open_elsewhere_error(&session_id, pid) {
                return Err(msg);
            }
            return Ok(info);
        }
        if let Some(msg) = sessions::session_open_elsewhere_error(&session_id, None) {
            return Err(msg);
        }
        {
            let mut attaching = self.inner.attaching_sessions.lock();
            if !attaching.insert(session_id.clone()) {
                return Err(format!("session {session_id} is already attaching").into());
            }
        }

        let _reservation = AttachReservation {
            inner: Arc::clone(&self.inner),
            session_id: session_id.clone(),
        };

        let permission_mode = match (req.permission_mode, req.always_approve) {
            (Some(m), _) => m,
            (None, Some(true)) => PermissionMode::BypassPermissions,
            // Session prefs → last-spawn → layered config (project-aware).
            (None, Some(false)) | (None, None) => {
                task_prefs::effective_permission_mode(Some(&session_id), Some(Path::new(&cwd)))
            }
        };
        let always_approve = permission_mode.spawns_always_approve();
        task_prefs::set_permission_mode(&session_id, permission_mode)?;
        let handle_id = Uuid::new_v4().to_string();
        let global_args = permission_mode.spawn_global_args();
        let agent_args = permission_mode.spawn_agent_args();

        let info = ManagedAgentInfo {
            handle_id: handle_id.clone(),
            session_id: Some(session_id.clone()),
            cwd: cwd.clone(),
            pid: None,
            status: ManagedStatus::Starting,
            permission_mode,
            always_approve,
            model_id: None,
            reasoning_effort: None,
            available_models: Vec::new(),
            last_error: None,
            title: Some(format!(
                "Attached {}",
                &session_id[..session_id.len().min(8)]
            )),
            created_at: now_iso(),
            pending_permission_count: 0,
        };
        let (mut info, client) = self.start_client(info, &global_args, &agent_args)?;
        let result = match client.session_load(&session_id, &cwd) {
            Ok(r) => r,
            Err(e) => {
                return Err(Self::fail_registered_agent(
                    &self.inner,
                    &handle_id,
                    &mut info,
                    e.user_message(),
                )
                .into());
            }
        };

        if let Some(models) = result.models.as_ref() {
            models::apply_models_info(&mut info, models);
        }
        info.status = ManagedStatus::Ready;
        if let Some(a) = self.inner.agents.lock().get_mut(&handle_id) {
            a.info = info.clone();
            a.last_activate = Some(Instant::now());
        }
        Self::emit_status(&self.inner, &info);

        Ok(info)
    }

    pub fn prompt(&self, handle_id: &str, text: &str) -> Result<Value, String> {
        if text.trim().is_empty() {
            return Err("prompt is empty".into());
        }
        let (session_id, client, generation) = {
            let agents = self.inner.agents.lock();
            let agent = agents
                .get(handle_id)
                .ok_or_else(|| format!("unknown handle {handle_id}"))?;
            if matches!(
                agent.info.status,
                ManagedStatus::Stopped
                    | ManagedStatus::Error
                    | ManagedStatus::Starting
                    | ManagedStatus::Stopping
            ) {
                return Err(format!("agent not ready ({:?})", agent.info.status));
            }
            let sid = agent
                .info
                .session_id
                .clone()
                .ok_or_else(|| "agent has no session_id".to_string())?;
            (sid, Arc::clone(&agent.client), agent.connection_generation)
        };

        self.dispatch_prompt(handle_id, &session_id, text.to_string(), client, generation);
        // Include post-dispatch status so the host can paint Running without
        // waiting on the agent-status event (avoids race with other upserts).
        let status = self
            .get(handle_id)
            .map(|a| a.status)
            .unwrap_or(ManagedStatus::Running);
        Ok(json!({
            "accepted": true,
            "handleId": handle_id,
            "sessionId": session_id,
            "status": status,
        }))
    }

    fn dispatch_prompt(
        &self,
        handle_id: &str,
        session_id: &str,
        text: String,
        client: Arc<AcpClient>,
        connection_generation: u64,
    ) {
        let prompt_id = Uuid::new_v4().to_string();
        {
            let mut agents = self.inner.agents.lock();
            let pending_count = pending_count(&self.inner, handle_id);
            if let Some(a) = agents.get_mut(handle_id) {
                if a.current_prompt_id.is_none() {
                    a.current_prompt_id = Some(prompt_id.clone());
                }
                a.in_flight_prompts.insert(prompt_id.clone());
                a.info.pending_permission_count = pending_count;
                if pending_count == 0 {
                    a.info.status = ManagedStatus::Running;
                }
                if a.info
                    .title
                    .as_ref()
                    .map(|t| t == "New agent" || t.starts_with("Attached "))
                    .unwrap_or(true)
                {
                    a.info.title = Some(truncate_text(&text, 80));
                }
                Self::emit_status(&self.inner, &a.info);
            }
        }

        let inner = Arc::clone(&self.inner);
        let handle_id = handle_id.to_string();
        let session_id = session_id.to_string();

        tauri::async_runtime::spawn_blocking(move || {
            let result = client.session_prompt(&session_id, &prompt_id, &text);
            if let Err(error) = &result {
                if let Some(failure) = error.transport_failure_tag() {
                    Self::handle_transport_closed(
                        &inner,
                        &handle_id,
                        connection_generation,
                        &error.user_message(),
                        failure,
                    );
                }
            }
            let mut agents = inner.agents.lock();
            if let Some(a) = agents.get_mut(&handle_id) {
                if a.connection_generation != connection_generation || a.reconnecting {
                    return;
                }
                let is_current = finish_prompt(
                    &mut a.in_flight_prompts,
                    &mut a.current_prompt_id,
                    &prompt_id,
                );
                match &result {
                    Ok(r) => {
                        if a.info.pending_permission_count == 0 && a.in_flight_prompts.is_empty() {
                            a.info.status = ManagedStatus::Ready;
                        } else if a.info.pending_permission_count == 0 {
                            a.info.status = ManagedStatus::Running;
                        }
                        if is_current {
                            a.info.last_error = None;
                        }
                        Self::emit(
                            &inner,
                            "agent-prompt-complete",
                            json!({
                                "handleId": handle_id,
                                "sessionId": session_id,
                                "promptId": prompt_id,
                                "result": r,
                            }),
                        );
                    }
                    Err(e) => {
                        if a.info.pending_permission_count == 0
                            && a.in_flight_prompts.is_empty()
                            && a.info.status != ManagedStatus::Error
                        {
                            a.info.status = ManagedStatus::Ready;
                        }
                        if is_current {
                            let message = e.user_message();
                            a.info.last_error = Some(message.clone());
                            Self::emit(
                                &inner,
                                "agent-prompt-complete",
                                json!({
                                    "handleId": handle_id,
                                    "sessionId": session_id,
                                    "promptId": prompt_id,
                                    "error": message,
                                }),
                            );
                        }
                    }
                }
                Self::emit_status(&inner, &a.info);
            }
        });
    }

    pub fn stop(&self, handle_id: &str) -> Result<ManagedAgentInfo, String> {
        self.inner.shell_stream.clear_handle(handle_id);

        let (session_id, client) = {
            let agents = self.inner.agents.lock();
            let agent = agents
                .get(handle_id)
                .ok_or_else(|| format!("unknown handle {handle_id}"))?;
            (agent.info.session_id.clone(), Arc::clone(&agent.client))
        };

        for entry in take_pending_for_handle(&self.inner, handle_id) {
            let permission = entry.permission;
            match permission.kind {
                PermissionKind::ToolPermission => {
                    let _ = entry.client.respond_result(
                        &permission.request_id,
                        json!({ "outcome": { "outcome": "cancelled" } }),
                    );
                }
                _ => {
                    let _ = entry.client.respond_error(
                        &permission.request_id,
                        -32000,
                        "Session stopped",
                    );
                }
            }
        }

        let had_running_turn = {
            let mut agents = self.inner.agents.lock();
            if let Some(agent) = agents.get_mut(handle_id) {
                let running = matches!(
                    agent.info.status,
                    ManagedStatus::Running | ManagedStatus::AwaitingPermission
                );
                agent.info.pending_permission_count = 0;
                agent.info.status = ManagedStatus::Stopping;
                Self::emit_status(&self.inner, &agent.info);
                running
            } else {
                false
            }
        };

        // Prefer ACP `session/close`: cancels the turn, all session subagents,
        // background tasks, and finalizes the replica. Fall back to
        // `session/cancel` on older agents that lack close.
        if let Some(sid) = &session_id {
            let close_delivered = match client.session_close(sid) {
                Ok(result) => {
                    if let Some(outcome) = result.close_outcome() {
                        tracing::info!(
                            handle_id,
                            session_id = %sid,
                            outcome,
                            "session/close completed"
                        );
                    }
                    true
                }
                Err(error) => {
                    tracing::warn!(
                        handle_id,
                        error = %error,
                        "session/close failed; falling back to session/cancel"
                    );
                    match client.session_cancel(sid, "user") {
                        Ok(()) => true,
                        Err(cancel_error) => {
                            tracing::warn!(
                                handle_id,
                                error = %cancel_error,
                                "graceful session cancellation failed; forcing shutdown"
                            );
                            false
                        }
                    }
                }
            };
            if had_running_turn && close_delivered {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut settled = false;
                while Instant::now() < deadline {
                    settled = self
                        .get(handle_id)
                        .map(|info| info.status != ManagedStatus::Stopping)
                        .unwrap_or(true);
                    if settled {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                if !settled {
                    tracing::warn!(handle_id, "forcing agent shutdown after close grace period");
                }
            }
        }

        let agent = {
            let mut agents = self.inner.agents.lock();
            agents
                .remove(handle_id)
                .ok_or_else(|| format!("unknown handle {handle_id}"))?
        };
        client.kill().map_err(|e| e.to_string())?;

        let mut info = agent.info;
        info.status = ManagedStatus::Stopped;
        info.pid = None;
        info.pending_permission_count = 0;
        Self::emit_status(&self.inner, &info);
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        can_reconnect_after_burst, finish_prompt, finish_prompt_status, next_reconnect_burst,
        terminal_prompt_id, ManagedStatus,
    };
    use serde_json::json;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    #[test]
    fn reconnect_burst_increments_when_last_activate_is_recent() {
        let now = Instant::now();
        let last = Some(now.checked_sub(Duration::from_millis(100)).unwrap());
        assert_eq!(next_reconnect_burst(last, now, 0), 1);
        assert_eq!(next_reconnect_burst(last, now, 1), 2);
        assert_eq!(next_reconnect_burst(last, now, 2), 3);
    }

    #[test]
    fn reconnect_burst_resets_when_last_activate_is_stale() {
        let now = Instant::now();
        let last = Some(now.checked_sub(Duration::from_secs(6)).unwrap());
        assert_eq!(next_reconnect_burst(last, now, 2), 0);
        assert_eq!(next_reconnect_burst(None, now, 2), 0);
    }

    #[test]
    fn reconnect_gives_up_after_three_unstable_deaths() {
        assert!(can_reconnect_after_burst(true, 0));
        assert!(can_reconnect_after_burst(true, 2));
        assert!(!can_reconnect_after_burst(true, 3));
        assert!(!can_reconnect_after_burst(false, 0));
    }

    #[test]
    fn queued_prompt_completion_does_not_replace_current_turn() {
        let mut in_flight = HashSet::from(["running".to_string(), "queued".to_string()]);
        let mut current = Some("running".to_string());

        assert!(!finish_prompt(&mut in_flight, &mut current, "queued"));
        assert_eq!(current.as_deref(), Some("running"));
        assert_eq!(in_flight, HashSet::from(["running".to_string()]));

        assert!(finish_prompt(&mut in_flight, &mut current, "running"));
        assert_eq!(current, None);
        assert!(in_flight.is_empty());
    }

    #[test]
    fn adopted_prompt_terminal_notification_returns_agent_to_ready() {
        let mut in_flight = HashSet::from(["adopted".to_string()]);
        let mut current = Some("adopted".to_string());

        assert_eq!(
            finish_prompt_status(&mut in_flight, &mut current, 0, "adopted"),
            Some(ManagedStatus::Ready)
        );
        assert!(in_flight.is_empty());
        assert_eq!(current, None);
    }

    #[test]
    fn prompt_complete_notification_identifies_terminal_prompt() {
        let params = json!({
            "sessionId": "session-1",
            "promptId": "prompt-live",
            "stopReason": "end_turn"
        });
        assert_eq!(
            terminal_prompt_id("x.ai/session/prompt_complete", &params),
            Some(Some("prompt-live"))
        );
        assert_eq!(
            terminal_prompt_id(
                "x.ai/session/prompt_complete",
                &json!({ "sessionId": "session-1" }),
            ),
            Some(None)
        );
    }

    #[test]
    fn durable_turn_completed_identifies_terminal_prompt() {
        let params = json!({
            "update": {
                "sessionUpdate": "turn_completed",
                "prompt_id": "prompt-live",
                "stop_reason": "end_turn"
            }
        });
        assert_eq!(
            terminal_prompt_id("session/update", &params),
            Some(Some("prompt-live"))
        );
        assert_eq!(
            terminal_prompt_id(
                "session/update",
                &json!({
                    "update": { "sessionUpdate": "agent_message_chunk" }
                })
            ),
            None
        );
    }
}
