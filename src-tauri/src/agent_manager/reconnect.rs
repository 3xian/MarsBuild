use super::{is_active_generation, pending_count, AgentManager, Inner, LiveAgent, RequestTarget};
use crate::acp::{AcpClient, NotifyFn};
use crate::agent_runtime::find_grok_bin;
use crate::agent_types::{ManagedStatus, PermissionMode};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const RECONNECT_DELAYS: [Duration; 5] = [
    Duration::from_millis(0),
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(5),
];

/// Messages from a reconnect candidate stay isolated until `session/load`
/// succeeds. Reverse RPC is the exception: the candidate may be blocked waiting
/// for its response, so it is routed explicitly to the candidate client.
struct CandidateInbox {
    inner: Arc<Inner>,
    handle_id: String,
    permission_mode: PermissionMode,
    session_id: String,
    client: Mutex<Option<Arc<AcpClient>>>,
    state: Mutex<CandidateState>,
}

#[derive(Default)]
struct CandidateState {
    active_generation: Option<u64>,
    buffered: Vec<Value>,
}

impl CandidateState {
    fn transport_closed_generation(&self) -> Option<u64> {
        self.active_generation
    }
}

impl CandidateInbox {
    fn new(
        inner: Arc<Inner>,
        handle_id: String,
        permission_mode: PermissionMode,
        session_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            handle_id,
            permission_mode,
            session_id,
            client: Mutex::new(None),
            state: Mutex::new(CandidateState::default()),
        })
    }

    fn notify(self: &Arc<Self>) -> NotifyFn {
        let inbox = Arc::clone(self);
        Arc::new(move |message| inbox.receive(message))
    }

    fn receive(&self, message: Value) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "_pinkcode/transport_closed" {
            let generation = self.state.lock().transport_closed_generation();
            if let Some(generation) = generation {
                let reason = message
                    .pointer("/params/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("ACP transport closed");
                let failure = message
                    .pointer("/params/failure")
                    .and_then(Value::as_str)
                    .unwrap_or("recv_failed");
                AgentManager::handle_transport_closed(
                    &self.inner,
                    &self.handle_id,
                    generation,
                    reason,
                    failure,
                );
            }
            return;
        }

        if is_reverse_request(&message) {
            if self
                .state
                .lock()
                .active_generation
                .is_some_and(|generation| {
                    !is_active_generation(&self.inner, &self.handle_id, generation)
                })
            {
                return;
            }
            if let Some(client) = self.client.lock().clone() {
                route_reverse_request(
                    &self.inner,
                    &self.handle_id,
                    &message,
                    client,
                    self.permission_mode,
                    &self.session_id,
                );
            } else {
                self.state.lock().buffered.push(message);
            }
            return;
        }

        let mut state = self.state.lock();
        if let Some(generation) = state.active_generation {
            drop(state);
            AgentManager::route_agent_message(&self.inner, &self.handle_id, generation, message);
        } else {
            state.buffered.push(message);
        }
    }

    fn bind_client(&self, client: Arc<AcpClient>) {
        *self.client.lock() = Some(Arc::clone(&client));
        let early_reverse = {
            let mut state = self.state.lock();
            let mut reverse = Vec::new();
            state.buffered.retain(|message| {
                if is_reverse_request(message) {
                    reverse.push(message.clone());
                    false
                } else {
                    true
                }
            });
            reverse
        };
        for message in early_reverse {
            route_reverse_request(
                &self.inner,
                &self.handle_id,
                &message,
                Arc::clone(&client),
                self.permission_mode,
                &self.session_id,
            );
        }
    }

    fn activate(&self, mut activation: CandidateActivation<'_>) -> Option<Arc<AcpClient>> {
        let generation = activation.next_generation;
        let (old_client, buffered) = {
            // Freeze the exact replay batch while deciding prompt adoption and
            // swapping the live generation. A terminal update cannot slip
            // between the replay summary and activation.
            let mut state = self.state.lock();
            if state.active_generation.is_some() {
                return None;
            }
            let buffered = std::mem::take(&mut state.buffered);
            let replay = ReplaySummary::from_messages(&buffered);
            activation.running_prompt_id = activation
                .running_prompt_id
                .take()
                .filter(|prompt_id| replay.should_adopt(prompt_id));
            let old_client = activate_candidate(&self.inner, &self.handle_id, activation)?;
            state.active_generation = Some(generation);
            (old_client, buffered)
        };

        for message in buffered {
            AgentManager::route_agent_message(&self.inner, &self.handle_id, generation, message);
        }
        Some(old_client)
    }
}

fn is_reverse_request(message: &Value) -> bool {
    message.get("id").is_some()
        && message.get("method").is_some()
        && message.get("result").is_none()
        && message.get("error").is_none()
}

fn route_reverse_request(
    inner: &Arc<Inner>,
    handle_id: &str,
    message: &Value,
    client: Arc<AcpClient>,
    permission_mode: PermissionMode,
    session_id: &str,
) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let request_id = message.get("id").cloned().unwrap_or(Value::Null);
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    AgentManager::dispatch_handle_request_to(
        inner,
        handle_id,
        request_id,
        method,
        params,
        RequestTarget {
            client,
            permission_mode,
            session_hint: Some(session_id.to_string()),
        },
    );
}

#[derive(Default)]
struct ReplaySummary {
    terminal_prompt_ids: HashSet<String>,
    rewound_prompt_ids: HashSet<String>,
}

impl ReplaySummary {
    fn from_messages(messages: &[Value]) -> Self {
        let mut summary = Self::default();
        for message in messages {
            let update = message.pointer("/params/update").unwrap_or(message);
            let kind = update
                .get("sessionUpdate")
                .and_then(Value::as_str)
                .unwrap_or("");
            if kind == "turn_completed" {
                if let Some(prompt_id) = prompt_id_from_update(message, update) {
                    summary.terminal_prompt_ids.insert(prompt_id.to_string());
                }
            } else if kind == "rewind_marker" {
                for field in [
                    "prompt_id",
                    "promptId",
                    "rewound_prompt_id",
                    "rewoundPromptId",
                ] {
                    if let Some(prompt_id) = update.get(field).and_then(Value::as_str) {
                        summary.rewound_prompt_ids.insert(prompt_id.to_string());
                    }
                }
            }
        }
        summary
    }

    fn should_adopt(&self, prompt_id: &str) -> bool {
        should_adopt_prompt_id(prompt_id)
            && !self.terminal_prompt_ids.contains(prompt_id)
            && !self.rewound_prompt_ids.contains(prompt_id)
    }
}

fn prompt_id_from_update<'a>(message: &'a Value, update: &'a Value) -> Option<&'a str> {
    update
        .get("prompt_id")
        .or_else(|| update.get("promptId"))
        .and_then(Value::as_str)
        .or_else(|| {
            message
                .pointer("/params/_meta/promptId")
                .and_then(Value::as_str)
        })
        .or_else(|| message.pointer("/_meta/promptId").and_then(Value::as_str))
}

/// Mirrors Grok Build's `should_adopt_running_prompt`: only user prompts and
/// scheduler-fired prompts have a reliable prompt-complete exit.
pub(super) fn should_adopt_prompt_id(prompt_id: &str) -> bool {
    prompt_id.starts_with("scheduler-fired-")
        || ![
            "task-completed-",
            "subagent-completed-",
            "workflow-completed-",
            "notifications-",
            "goal-summary-",
            "goal-classifier-nudge-",
            "plan-resume-",
        ]
        .iter()
        .any(|prefix| prompt_id.starts_with(prefix))
}

pub(super) fn spawn(inner: Arc<Inner>, handle_id: String, failed_generation: u64) {
    let worker_inner = Arc::clone(&inner);
    let worker_handle_id = handle_id.clone();
    let result = thread::Builder::new()
        .name(format!(
            "acp-reconnect-{}",
            &handle_id[..handle_id.len().min(8)]
        ))
        .spawn(move || run(worker_inner, worker_handle_id, failed_generation));

    if let Err(error) = result {
        fail_worker_start(&inner, &handle_id, failed_generation, &error);
    }
}

fn run(inner: Arc<Inner>, handle_id: String, failed_generation: u64) {
    let next_generation = failed_generation.wrapping_add(1);
    let mut last_error = "ACP reconnect did not start".to_string();

    // Reap the failed stdio child before spawning a replacement. Overlapping
    // `grok agent stdio` processes on the same session fight over session
    // locks and MCP, which shows up as the task card flipping Live ↔ Starting.
    let killed_pid = kill_failed_client(&inner, &handle_id, failed_generation);
    if let Some(pid) = killed_pid {
        let _ = crate::sessions::wait_until_dead(pid, Duration::from_secs(2));
    }

    if let Some(snapshot) = reconnect_snapshot(&inner, &handle_id, failed_generation) {
        if let Some(err) =
            crate::sessions::session_open_elsewhere_error(&snapshot.session_id, killed_pid)
        {
            fail_reconnect(&inner, &handle_id, failed_generation, &err.message);
            return;
        }
    }

    for delay in RECONNECT_DELAYS {
        if !delay.is_zero() {
            thread::sleep(delay);
        }

        let Some(snapshot) = reconnect_snapshot(&inner, &handle_id, failed_generation) else {
            return;
        };
        let grok_bin = if Path::new(&snapshot.grok_bin).is_file() {
            snapshot.grok_bin
        } else {
            find_grok_bin().unwrap_or(snapshot.grok_bin)
        };
        let inbox = CandidateInbox::new(
            Arc::clone(&inner),
            handle_id.clone(),
            snapshot.permission_mode,
            snapshot.session_id.clone(),
        );
        let candidate = match AcpClient::spawn_with_notify(
            &grok_bin,
            snapshot.always_approve,
            &snapshot.global_args,
            &snapshot.agent_args,
            inbox.notify(),
        ) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                last_error = error.user_message();
                continue;
            }
        };
        inbox.bind_client(Arc::clone(&candidate));

        let initialized = match candidate.initialize() {
            Ok(value) => value,
            Err(error) => {
                last_error = error.user_message();
                discard_candidate_permissions(&inner, &handle_id, &candidate);
                let _ = candidate.kill();
                continue;
            }
        };
        if let Err(error) = candidate.authenticate_if_available(&initialized) {
            tracing::warn!(
                handle_id,
                error = %error,
                "ACP reconnect authentication failed; trying session/load"
            );
        }
        let loaded = match candidate.session_load(&snapshot.session_id, &snapshot.cwd) {
            Ok(value) => value,
            Err(error) => {
                last_error = error.user_message();
                discard_candidate_permissions(&inner, &handle_id, &candidate);
                let _ = candidate.kill();
                continue;
            }
        };

        let activation = CandidateActivation {
            failed_generation,
            next_generation,
            grok_bin: &grok_bin,
            candidate: &candidate,
            models_info: loaded.models.clone(),
            running_prompt_id: loaded.running_prompt_id().map(str::to_string),
        };
        let Some(old_client) = inbox.activate(activation) else {
            discard_candidate_permissions(&inner, &handle_id, &candidate);
            let _ = candidate.kill();
            return;
        };

        AgentManager::notify_mode_changed(&candidate, snapshot.permission_mode);
        AgentManager::drain_early_requests(&inner, &handle_id);
        let _ = old_client.kill();
        if !is_active_generation(&inner, &handle_id, next_generation) {
            return;
        }
        AgentManager::emit(
            &inner,
            "agent-reconnected",
            json!({
                "handleId": handle_id,
                "sessionId": snapshot.session_id,
                "pid": candidate.pid(),
            }),
        );
        return;
    }

    fail_reconnect(&inner, &handle_id, failed_generation, &last_error);
}

struct ReconnectSnapshot {
    session_id: String,
    cwd: String,
    always_approve: bool,
    permission_mode: PermissionMode,
    grok_bin: String,
    global_args: Vec<String>,
    agent_args: Vec<String>,
}

fn kill_failed_client(inner: &Inner, handle_id: &str, failed_generation: u64) -> Option<u32> {
    let client = {
        let agents = inner.agents.lock();
        let agent = agents.get(handle_id)?;
        if agent.connection_generation != failed_generation || !agent.reconnecting {
            return None;
        }
        Arc::clone(&agent.client)
    };
    let pid = client.pid();
    let _ = client.kill();
    Some(pid)
}

fn reconnect_snapshot(
    inner: &Inner,
    handle_id: &str,
    failed_generation: u64,
) -> Option<ReconnectSnapshot> {
    let agents = inner.agents.lock();
    let agent = agents.get(handle_id)?;
    if agent.connection_generation != failed_generation
        || !agent.reconnecting
        || matches!(
            agent.info.status,
            ManagedStatus::Stopping | ManagedStatus::Stopped | ManagedStatus::Error
        )
    {
        return None;
    }
    Some(ReconnectSnapshot {
        session_id: agent.info.session_id.clone()?,
        cwd: agent.info.cwd.clone(),
        always_approve: agent.info.always_approve,
        permission_mode: agent.info.permission_mode,
        grok_bin: agent.grok_bin.clone(),
        global_args: agent.global_args.clone(),
        agent_args: agent.agent_args.clone(),
    })
}

struct CandidateActivation<'a> {
    failed_generation: u64,
    next_generation: u64,
    grok_bin: &'a str,
    candidate: &'a Arc<AcpClient>,
    models_info: Option<crate::acp::protocol::SessionModelsInfo>,
    running_prompt_id: Option<String>,
}

fn activate_candidate(
    inner: &Inner,
    handle_id: &str,
    activation: CandidateActivation<'_>,
) -> Option<Arc<AcpClient>> {
    let mut agents = inner.agents.lock();
    let agent = agents.get_mut(handle_id)?;
    if agent.connection_generation != activation.failed_generation || !agent.reconnecting {
        return None;
    }
    let old_client = std::mem::replace(&mut agent.client, Arc::clone(activation.candidate));
    agent.connection_generation = activation.next_generation;
    agent.reconnecting = false;
    agent.grok_bin = activation.grok_bin.to_string();
    agent.info.pid = Some(activation.candidate.pid());
    agent.info.last_error = None;
    agent.info.pending_permission_count = pending_count(inner, handle_id);
    if let Some(ref models) = activation.models_info {
        super::models::apply_models_info(&mut agent.info, models);
    }
    set_prompt_state(agent, activation.running_prompt_id);
    agent.last_activate = Some(std::time::Instant::now());
    AgentManager::emit_status(inner, &agent.info);
    Some(old_client)
}

fn set_prompt_state(agent: &mut LiveAgent, running_prompt_id: Option<String>) {
    agent.in_flight_prompts.clear();
    agent.current_prompt_id = running_prompt_id.clone();
    if let Some(prompt_id) = running_prompt_id {
        agent.in_flight_prompts.insert(prompt_id);
        agent.info.status = if agent.info.pending_permission_count > 0 {
            ManagedStatus::AwaitingPermission
        } else {
            ManagedStatus::Running
        };
    } else if agent.info.pending_permission_count > 0 {
        agent.info.status = ManagedStatus::AwaitingPermission;
    } else {
        agent.info.status = ManagedStatus::Ready;
    }
}

fn discard_candidate_permissions(inner: &Inner, handle_id: &str, candidate: &Arc<AcpClient>) {
    let cancelled = {
        let mut pending = inner.pending.lock();
        let keys: Vec<String> = pending
            .iter()
            .filter(|(_, entry)| Arc::ptr_eq(&entry.client, candidate))
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .filter_map(|key| pending.remove(&key))
            .collect::<Vec<_>>()
    };
    if cancelled.is_empty() {
        return;
    }

    for entry in cancelled {
        if let Ok(value) = serde_json::to_value(&entry.permission) {
            AgentManager::emit(
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

    let updated = {
        let mut agents = inner.agents.lock();
        agents.get_mut(handle_id).and_then(|agent| {
            if !agent.reconnecting {
                return None;
            }
            agent.info.pending_permission_count = pending_count(inner, handle_id);
            agent.info.status = ManagedStatus::Starting;
            Some(agent.info.clone())
        })
    };
    if let Some(info) = updated {
        AgentManager::emit_status(inner, &info);
    }
}

fn fail_reconnect(inner: &Inner, handle_id: &str, failed_generation: u64, error: &str) {
    let updated = {
        let mut agents = inner.agents.lock();
        agents.get_mut(handle_id).and_then(|agent| {
            if agent.connection_generation != failed_generation || !agent.reconnecting {
                return None;
            }
            agent.reconnecting = false;
            agent.in_flight_prompts.clear();
            agent.current_prompt_id = None;
            agent.info.status = ManagedStatus::Error;
            agent.info.pid = None;
            agent.info.last_error = Some(format!("ACP reconnect failed: {error}"));
            Some((agent.info.clone(), Arc::clone(&agent.client)))
        })
    };
    if let Some((info, client)) = updated {
        AgentManager::emit_status(inner, &info);
        let _ = client.kill();
    }
}

fn fail_worker_start(
    inner: &Inner,
    handle_id: &str,
    failed_generation: u64,
    error: &std::io::Error,
) {
    let updated = {
        let mut agents = inner.agents.lock();
        agents.get_mut(handle_id).and_then(|agent| {
            if agent.connection_generation != failed_generation {
                return None;
            }
            agent.reconnecting = false;
            agent.info.status = ManagedStatus::Error;
            agent.info.last_error = Some(format!("failed to start ACP reconnect worker: {error}"));
            Some(agent.info.clone())
        })
    };
    if let Some(info) = updated {
        AgentManager::emit_status(inner, &info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_adoption_matches_grok_build_origin_rules() {
        assert!(should_adopt_prompt_id("user-prompt"));
        assert!(should_adopt_prompt_id("scheduler-fired-loop-1"));
        for prompt_id in [
            "task-completed-task-1",
            "subagent-completed-agent-1",
            "workflow-completed-run-1",
            "notifications-batch-1",
            "goal-summary-goal-1",
            "goal-classifier-nudge-goal-1",
            "plan-resume-1",
        ] {
            assert!(!should_adopt_prompt_id(prompt_id), "{prompt_id}");
        }
    }

    #[test]
    fn replay_terminal_prevents_running_prompt_adoption() {
        let replay = ReplaySummary::from_messages(&[json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "turn_completed",
                    "prompt_id": "prompt-finished",
                    "stop_reason": "end_turn"
                }
            }
        })]);
        assert!(!replay.should_adopt("prompt-finished"));
        assert!(replay.should_adopt("prompt-still-running"));
    }

    #[test]
    fn replay_rewind_with_prompt_id_prevents_adoption() {
        let replay = ReplaySummary::from_messages(&[json!({
            "method": "session/update",
            "params": {
                "update": {
                    "sessionUpdate": "rewind_marker",
                    "rewound_prompt_id": "prompt-rewound"
                }
            }
        })]);
        assert!(!replay.should_adopt("prompt-rewound"));
    }

    #[test]
    fn candidate_transport_close_routes_after_activation() {
        let mut state = CandidateState::default();
        assert_eq!(state.transport_closed_generation(), None);

        state.active_generation = Some(2);
        assert_eq!(state.transport_closed_generation(), Some(2));
    }
}
