//! ACP (Agent Client Protocol) JSON-RPC client over stdio.
//!
//! Architecture:
//! - [`gateway`] — channel-based I/O actor (owns stdin, demuxes responses)
//! - [`protocol`] — type-safe request/response payloads (no hand-rolled maps)

mod gateway;
pub mod protocol;

use gateway::AcpGateway;
use protocol::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

pub use protocol::CLIENT_IDENTIFIER;

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("rpc error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("timeout waiting for response to {0}")]
    Timeout(String),
    #[error("ACP channel send failed: {0}")]
    SendFailed(String),
    #[error("ACP channel receive failed: {0}")]
    RecvFailed(String),
    #[error("ACP transport closed: {0}")]
    TransportClosed(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AcpError>;

impl AcpError {
    pub fn transport_failure_tag(&self) -> Option<&'static str> {
        match self {
            Self::SendFailed(_) => Some("send_failed"),
            Self::RecvFailed(_) | Self::TransportClosed(_) => Some("recv_failed"),
            _ => None,
        }
    }

    /// Stable, actionable text for UI surfaces. The error itself retains the
    /// original protocol fields so logs and callers can still inspect them.
    pub fn user_message(&self) -> String {
        match self {
            Self::Rpc {
                code,
                message,
                data,
            } => user_facing_rpc_message(*code, message, data.as_ref()),
            _ => self.to_string(),
        }
    }
}

/// Callback for agent → client notifications / unsolicited messages.
pub type NotifyFn = Arc<dyn Fn(Value) + Send + Sync + 'static>;

/// High-level ACP client. All I/O goes through the channel gateway.
pub struct AcpClient {
    gateway: AcpGateway,
}

impl AcpClient {
    /// Spawn `grok [global_args…] agent [agent flags…] stdio`.
    ///
    /// `global_args` must come **before** `agent` (e.g. `--permission-mode auto`).
    /// `agent_args` come after `agent` and before `stdio` (e.g. `-m model`).
    pub fn spawn_with_notify(
        grok_bin: &str,
        always_approve: bool,
        global_args: &[String],
        agent_args: &[String],
        on_notify: NotifyFn,
    ) -> Result<Self> {
        let args = build_spawn_argv(always_approve, global_args, agent_args);

        let mut cmd = Command::new(grok_bin);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Propagate proxy so grok can reach xAI APIs behind firewalls/proxies.
        if let Some(proxy_url) = crate::proxy::detect_proxy() {
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                cmd.env(key, &proxy_url);
            }
        }

        // GUI host on Windows: hide the console window that `grok.exe` would open.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let child = cmd.spawn()?;
        let gateway = AcpGateway::start(child, on_notify)?;
        Ok(Self { gateway })
    }

    pub fn pid(&self) -> u32 {
        self.gateway.pid()
    }

    /// Low-level typed call: serialize params, deserialize result.
    pub fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &'static str,
        params: &P,
        timeout: Duration,
    ) -> Result<R> {
        let params_val = serde_json::to_value(params)?;
        let result = self.gateway.call(method, params_val, timeout)?;
        Ok(serde_json::from_value(result)?)
    }

    /// Low-level typed call that keeps the raw JSON result (flexible responses).
    pub fn call_raw<P: Serialize>(
        &self,
        method: &'static str,
        params: &P,
        timeout: Duration,
    ) -> Result<Value> {
        let params_val = serde_json::to_value(params)?;
        self.gateway.call(method, params_val, timeout)
    }

    /// Ext method that returns Grok's `{ result, error? }` envelope.
    pub fn call_ext<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &'static str,
        params: &P,
        timeout: Duration,
    ) -> Result<R> {
        let raw = self.call_raw(method, params, timeout)?;
        let envelope: ExtMethodEnvelope<R> = serde_json::from_value(raw)?;
        envelope.into_result().map_err(AcpError::Other)
    }

    /// Typed JSON-RPC notification (no response).
    pub fn notify_typed<P: Serialize>(&self, method: &'static str, params: &P) -> Result<()> {
        let envelope = JsonRpcNotification {
            jsonrpc: "2.0",
            method,
            params,
        };
        let line = serde_json::to_string(&envelope)?;
        self.gateway.write_line(line)
    }

    /// Respond to a server→client JSON-RPC request.
    pub fn respond_result(&self, id: &Value, result: Value) -> Result<()> {
        let envelope = JsonRpcResultResponse {
            jsonrpc: "2.0",
            id: id.clone(),
            result,
        };
        let line = serde_json::to_string(&envelope)?;
        self.gateway.write_line(line)
    }

    pub fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<()> {
        let envelope = JsonRpcErrorResponse {
            jsonrpc: "2.0",
            id: id.clone(),
            error: JsonRpcErrorBody {
                code,
                message: message.to_string(),
            },
        };
        let line = serde_json::to_string(&envelope)?;
        self.gateway.write_line(line)
    }

    pub fn initialize(&self) -> Result<InitializeResult> {
        self.call(
            "initialize",
            &InitializeParams::pinkcode(),
            Duration::from_secs(30),
        )
    }

    /// Complete the ACP authentication handshake when initialize advertises a
    /// non-interactive cached-token or API-key method.
    pub fn authenticate_if_available(
        &self,
        initialize: &InitializeResult,
    ) -> Result<Option<Value>> {
        let Some(method_id) = select_non_interactive_auth_method(initialize) else {
            return Ok(None);
        };
        // Align with Grok `STARTUP_AUTH_TIMEOUT` (60s) — cold token refresh.
        self.call_raw(
            "authenticate",
            &AuthenticateParams {
                method_id: method_id.to_string(),
            },
            Duration::from_secs(60),
        )
        .map(Some)
    }

    pub fn session_new(&self, cwd: &str) -> Result<SessionBootstrapResult> {
        self.call(
            "session/new",
            &SessionNewParams {
                cwd: cwd.to_string(),
                mcp_servers: vec![],
            },
            Duration::from_secs(60),
        )
    }

    pub fn session_load(&self, session_id: &str, cwd: &str) -> Result<SessionBootstrapResult> {
        // Grok `LoadSessionResponse` often omits `sessionId` (client already
        // knows it). Normalize via the shared helper so the field is stable.
        // 120s covers large-session load under streaming replay (see
        // docs/grok-build-watch · MIN_CLIENT_CONNECT_TIMEOUT floor).
        let mut result: SessionBootstrapResult = self.call(
            "session/load",
            &SessionLoadParams {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
                mcp_servers: vec![],
            },
            Duration::from_secs(120),
        )?;
        result.session_id = result.resolve_session_id(Some(session_id));
        Ok(result)
    }

    pub fn session_prompt(&self, session_id: &str, prompt_id: &str, text: &str) -> Result<Value> {
        self.call_raw(
            "session/prompt",
            &SessionPromptParams::text(session_id, prompt_id, text),
            Duration::from_secs(60 * 30),
        )
    }

    /// ACP `session/set_mode` — switch agent operating mode (e.g. `plan`).
    pub fn session_set_mode(&self, session_id: &str, mode_id: &str) -> Result<Value> {
        self.call_raw(
            "session/set_mode",
            &SessionSetModeParams {
                session_id: session_id.to_string(),
                mode_id: mode_id.to_string(),
            },
            Duration::from_secs(30),
        )
    }

    /// Queue a user interjection into the currently running Grok turn.
    pub fn session_interject(&self, session_id: &str, text: &str) -> Result<Value> {
        self.call_raw(
            "x.ai/interject",
            &InterjectParams {
                session_id: session_id.to_string(),
                text: text.to_string(),
                interjection_id: uuid::Uuid::new_v4().to_string(),
            },
            Duration::from_secs(30),
        )
    }

    pub fn queue_remove(&self, session_id: &str, id: &str, expected_version: u64) -> Result<()> {
        self.notify_typed(
            "x.ai/queue/remove",
            &QueueRemoveParams {
                session_id: session_id.to_string(),
                id: id.to_string(),
                expected_version,
                client_identifier: CLIENT_IDENTIFIER.into(),
            },
        )
    }

    pub fn queue_reorder(&self, session_id: &str, ordered_ids: &[String]) -> Result<()> {
        self.notify_typed(
            "x.ai/queue/reorder",
            &QueueReorderParams {
                session_id: session_id.to_string(),
                ordered_ids: ordered_ids.to_vec(),
                client_identifier: CLIENT_IDENTIFIER.into(),
            },
        )
    }

    pub fn queue_clear(&self, session_id: &str) -> Result<()> {
        self.notify_typed(
            "x.ai/queue/clear",
            &QueueClearParams {
                session_id: session_id.to_string(),
                client_identifier: CLIENT_IDENTIFIER.into(),
            },
        )
    }

    pub fn queue_edit(&self, session_id: &str, id: &str, new_text: &str) -> Result<()> {
        self.notify_typed(
            "x.ai/queue/edit",
            &QueueEditParams {
                session_id: session_id.to_string(),
                id: id.to_string(),
                new_text: new_text.to_string(),
                client_identifier: CLIENT_IDENTIFIER.into(),
            },
        )
    }

    pub fn queue_interject(&self, session_id: &str, id: &str, expected_version: u64) -> Result<()> {
        self.notify_typed(
            "x.ai/queue/interject",
            &QueueInterjectParams {
                session_id: session_id.to_string(),
                id: id.to_string(),
                expected_version,
                client_identifier: CLIENT_IDENTIFIER.into(),
            },
        )
    }

    /// ACP `session/set_model` — switch the session model.
    pub fn set_session_model(
        &self,
        session_id: &str,
        model_id: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<SetSessionModelResult> {
        self.call(
            "session/set_model",
            &SetSessionModelParams::new(session_id, model_id)
                .with_reasoning_effort(reasoning_effort),
            Duration::from_secs(30),
        )
    }

    /// ACP `x.ai/session/usage` — cumulative session tokens/cost (nested `usage`).
    pub fn session_usage(&self, session_id: &str) -> Result<SessionUsageResult> {
        let wire: SessionUsageWire = self.call(
            "x.ai/session/usage",
            &SessionUsageParams {
                session_id: session_id.to_string(),
            },
            Duration::from_secs(15),
        )?;
        Ok(SessionUsageResult::from(wire))
    }

    /// ACP `x.ai/recap` — request a recap (fire-and-forget; text arrives as notification).
    pub fn recap(&self, session_id: &str, auto: bool) -> Result<RecapResult> {
        self.call(
            "x.ai/recap",
            &RecapParams {
                session_id: session_id.to_string(),
                auto,
            },
            Duration::from_secs(30),
        )
    }

    /// ACP `x.ai/rewind/points` — list rewritable prompt indices.
    pub fn rewind_points(&self, session_id: &str) -> Result<RewindPointsResult> {
        self.call(
            "x.ai/rewind/points",
            &RewindPointsParams {
                session_id: session_id.to_string(),
            },
            Duration::from_secs(15),
        )
    }

    /// ACP `x.ai/rewind/execute` — rewind to a previous prompt index.
    pub fn rewind_execute(
        &self,
        session_id: &str,
        target_prompt_index: u64,
        mode: Option<&str>,
    ) -> Result<RewindExecuteResult> {
        self.call(
            "x.ai/rewind/execute",
            &RewindExecuteParams {
                session_id: session_id.to_string(),
                target_prompt_index,
                mode: mode.map(|s| s.to_string()),
                force: false,
            },
            Duration::from_secs(30),
        )
    }

    /// ACP `x.ai/subagent/cancel` — cancel a running subagent.
    pub fn cancel_subagent(
        &self,
        session_id: &str,
        subagent_id: &str,
    ) -> Result<CancelSubagentResult> {
        self.call_ext(
            "x.ai/subagent/cancel",
            &CancelSubagentParams {
                session_id: Some(session_id.to_string()),
                subagent_id: subagent_id.to_string(),
            },
            Duration::from_secs(15),
        )
    }

    /// ACP `x.ai/subagent/list_running` — list running subagents for a session.
    pub fn list_subagents(&self, session_id: &str) -> Result<ListSubagentsResult> {
        self.call_ext(
            "x.ai/subagent/list_running",
            &ListSubagentsParams {
                session_id: session_id.to_string(),
            },
            Duration::from_secs(15),
        )
    }

    /// ACP `x.ai/task/kill` — kill a background task.
    pub fn kill_task(&self, session_id: &str, task_id: &str) -> Result<KillTaskResult> {
        self.call_ext(
            "x.ai/task/kill",
            &KillTaskParams {
                session_id: session_id.to_string(),
                task_id: task_id.to_string(),
            },
            Duration::from_secs(15),
        )
    }

    /// ACP `x.ai/task/list` — list background tasks for a session.
    pub fn list_tasks(&self, session_id: &str) -> Result<ListTasksResult> {
        self.call_ext(
            "x.ai/task/list",
            &ListTasksParams {
                session_id: session_id.to_string(),
            },
            Duration::from_secs(15),
        )
    }

    /// Host permission mode → Grok shell yolo/auto notification.
    pub fn notify_yolo_mode(
        &self,
        yolo_mode: bool,
        auto_mode: bool,
        permission_mode: &'static str,
    ) -> Result<()> {
        self.notify_typed(
            "x.ai/yolo_mode_changed",
            &YoloModeChangedParams {
                yolo_mode,
                auto_mode,
                permission_mode,
            },
        )
    }

    /// ACP `session/cancel` — cancel in-flight turn gracefully.
    pub fn session_cancel(&self, session_id: &str, reason: &str) -> Result<()> {
        self.notify_typed(
            "session/cancel",
            &SessionCancelParams {
                session_id: session_id.to_string(),
                reason: reason.to_string(),
            },
        )
    }

    /// ACP `session/close` — cancel turn/subagents/bg tasks and finalize the
    /// session replica before the host tears down the transport.
    pub fn session_close(&self, session_id: &str) -> Result<SessionCloseResult> {
        self.call(
            "session/close",
            &SessionCloseParams {
                session_id: session_id.to_string(),
            },
            // Close waits behind prompt intake and drains children; align with
            // Grok's multi-stage close budget rather than a short notify timeout.
            Duration::from_secs(30),
        )
    }

    pub fn kill(&self) -> Result<()> {
        self.gateway.kill()
    }
}

fn select_non_interactive_auth_method(initialize: &InitializeResult) -> Option<&str> {
    let default = initialize.default_auth_method_id();
    if let Some(id @ ("cached_token" | "xai.api_key")) = default {
        if initialize.has_auth_method(id) {
            return Some(id);
        }
    }
    ["cached_token", "xai.api_key"]
        .into_iter()
        .find(|id| initialize.has_auth_method(id))
}

/// ACP keeps its stable JSON-RPC label in `error.message` and puts the useful
/// upstream failure in `error.data`. Prefer that detail for UI-facing errors.
fn user_facing_rpc_message(code: i64, message: &str, data: Option<&Value>) -> String {
    let detail = data.and_then(rpc_error_detail);
    let http_status = data
        .and_then(|value| value.get("http_status").or_else(|| value.get("httpStatus")))
        .and_then(Value::as_u64);

    if http_status == Some(402)
        || detail.is_some_and(|text| {
            let lower = text.to_ascii_lowercase();
            lower.contains("402") && lower.contains("usage balance exhausted")
        })
    {
        return "Grok Build usage balance exhausted. Add credits or switch to an account with available balance. (HTTP 402 Payment Required)".into();
    }

    if let Some(detail) = detail.filter(|detail| !detail.trim().is_empty()) {
        return detail.trim().to_string();
    }

    if message.eq_ignore_ascii_case("internal error") {
        return format!("Grok Build returned an internal error (RPC {code}).");
    }

    message.to_string()
}

fn rpc_error_detail(data: &Value) -> Option<&str> {
    data.as_str()
        .or_else(|| data.get("message").and_then(Value::as_str))
        .or_else(|| data.pointer("/error/message").and_then(Value::as_str))
}

/// Build the argv passed to `grok` for ACP stdio (production spawn + tests).
fn build_spawn_argv(
    always_approve: bool,
    global_args: &[String],
    agent_args: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.extend(global_args.iter().cloned());
    args.push("agent".into());
    // Dedicated per-task process. Sharing the TUI leader makes session/load
    // steal the same backend and the card heading flip Live ↔ Starting.
    args.push("--no-leader".into());
    if always_approve {
        args.push("--always-approve".into());
    }
    args.extend(agent_args.iter().cloned());
    args.push("stdio".into());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    #[test]
    fn transport_close_wakes_all_pending_requests() {
        let pending = parking_lot::Mutex::new(std::collections::HashMap::new());
        let (tx1, rx1) = mpsc::channel();
        let (tx2, rx2) = mpsc::channel();
        pending.lock().insert(10, gateway::pending_with(tx1));
        pending.lock().insert(11, gateway::pending_with(tx2));

        gateway::fail_all_pending_for_test(&pending, "closed");

        assert!(pending.lock().is_empty());
        match rx1.recv().expect("first waiter") {
            Err(AcpError::RecvFailed(msg)) => assert_eq!(msg, "closed"),
            other => panic!("unexpected {other:?}"),
        }
        match rx2.recv().expect("second waiter") {
            Err(AcpError::RecvFailed(msg)) => assert_eq!(msg, "closed"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn transport_closed_message_includes_stderr_tail() {
        let empty = parking_lot::Mutex::new(VecDeque::new());
        assert_eq!(
            gateway::transport_closed_message_for_test(&empty),
            "ACP transport closed"
        );

        let mut tail = VecDeque::new();
        tail.push_back("error: unexpected argument '--permission-mode' found".into());
        let with = parking_lot::Mutex::new(tail);
        let msg = gateway::transport_closed_message_for_test(&with);
        assert!(msg.starts_with("ACP transport closed: "));
        assert!(msg.contains("unexpected argument"));
    }

    #[test]
    fn rpc_error_uses_structured_detail_instead_of_internal_error_label() {
        let data = json!({
            "message": "API error (status 500): upstream unavailable",
            "http_status": 500
        });
        assert_eq!(
            user_facing_rpc_message(-32603, "Internal error", Some(&data)),
            "API error (status 500): upstream unavailable"
        );
    }

    #[test]
    fn rpc_error_explains_exhausted_grok_build_balance() {
        let data = json!({
            "message": "API error (status 402 Payment Required): Grok Build usage balance exhausted",
            "http_status": 402
        });
        let error = AcpError::Rpc {
            code: -32603,
            message: "Internal error".into(),
            data: Some(data),
        };
        assert_eq!(error.to_string(), "rpc error -32603: Internal error");
        assert_eq!(
            error.user_message(),
            "Grok Build usage balance exhausted. Add credits or switch to an account with available balance. (HTTP 402 Payment Required)"
        );
    }

    #[test]
    fn rpc_error_keeps_code_when_no_detail_is_available() {
        assert_eq!(
            user_facing_rpc_message(-32603, "Internal error", None),
            "Grok Build returned an internal error (RPC -32603)."
        );
    }

    #[test]
    fn transport_failure_classification_preserves_direction() {
        assert_eq!(
            AcpError::SendFailed("closed".into()).transport_failure_tag(),
            Some("send_failed")
        );
        assert_eq!(
            AcpError::RecvFailed("closed".into()).transport_failure_tag(),
            Some("recv_failed")
        );
        assert_eq!(
            AcpError::Timeout("method".into()).transport_failure_tag(),
            None
        );
    }

    #[test]
    fn spawn_argv_places_permission_mode_before_agent() {
        let global = vec!["--permission-mode".into(), "auto".into()];
        let agent = vec!["-m".into(), "grok-4".into()];
        assert_eq!(
            build_spawn_argv(false, &global, &agent),
            vec![
                "--permission-mode",
                "auto",
                "agent",
                "--no-leader",
                "-m",
                "grok-4",
                "stdio",
            ]
        );
        assert_eq!(
            build_spawn_argv(true, &[], &[]),
            vec!["agent", "--no-leader", "--always-approve", "stdio"]
        );
    }

    #[test]
    fn auth_selection_prefers_agent_default_but_skips_interactive_methods() {
        let initialized: InitializeResult = serde_json::from_value(json!({
            "authMethods": [
                { "id": "grok.com", "name": "Grok" },
                { "id": "xai.api_key", "name": "API key" },
                { "id": "cached_token", "name": "Cached token" }
            ],
            "_meta": { "defaultAuthMethodId": "xai.api_key" }
        }))
        .unwrap();
        assert_eq!(
            select_non_interactive_auth_method(&initialized),
            Some("xai.api_key")
        );

        let interactive_only: InitializeResult = serde_json::from_value(json!({
            "authMethods": [{ "id": "grok.com", "name": "Grok" }],
            "_meta": { "defaultAuthMethodId": "grok.com" }
        }))
        .unwrap();
        assert_eq!(select_non_interactive_auth_method(&interactive_only), None);
    }

    #[test]
    fn initialize_params_serialize_with_client_identifier_meta() {
        let v = serde_json::to_value(InitializeParams::pinkcode()).unwrap();
        assert_eq!(v["protocolVersion"], 1);
        assert_eq!(v["clientInfo"]["name"], "pinkcode");
        assert_eq!(v["meta"]["clientIdentifier"], CLIENT_IDENTIFIER);
        assert_eq!(
            v["clientCapabilities"]["meta"]["x.ai/incrementalBashOutput"],
            true
        );
    }

    #[test]
    fn set_session_model_params_use_meta_for_effort() {
        let p = SetSessionModelParams::new("s1", "grok-4").with_reasoning_effort(Some("high"));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["sessionId"], "s1");
        assert_eq!(v["modelId"], "grok-4");
        assert_eq!(v["_meta"]["reasoningEffort"], "high");
        assert!(v.get("reasoningEffort").is_none());
    }

    #[test]
    fn session_usage_wire_nests_under_usage() {
        let wire: SessionUsageWire = serde_json::from_value(json!({
            "usage": {
                "inputTokens": 100,
                "outputTokens": 10,
                "numTurns": 2,
                "costUsdTicks": 20_000_000
            }
        }))
        .unwrap();
        let flat = SessionUsageResult::from(wire);
        assert_eq!(flat.input_tokens, 100);
        assert_eq!(flat.output_tokens, 10);
        assert_eq!(flat.total_tokens, 110);
        assert_eq!(flat.turn_count, 2);
        assert_eq!(flat.cost_usd_ticks, 20_000_000);
    }

    #[test]
    fn ext_method_envelope_unwraps_result() {
        let raw = json!({ "result": { "taskId": "t1", "outcome": "killed" } });
        let env: ExtMethodEnvelope<KillTaskResult> = serde_json::from_value(raw).unwrap();
        let r = env.into_result().unwrap();
        assert_eq!(r.task_id.as_deref(), Some("t1"));
        assert_eq!(r.outcome, Some(json!("killed")));
    }

    #[test]
    fn rewind_points_use_snake_case_wire() {
        let r: RewindPointsResult = serde_json::from_value(json!({
            "rewind_points": [{
                "prompt_index": 3,
                "created_at": "2026-01-01T00:00:00Z",
                "num_file_snapshots": 2,
                "has_file_changes": true,
                "prompt_preview": "fix auth"
            }]
        }))
        .unwrap();
        assert_eq!(r.rewind_points.len(), 1);
        assert_eq!(r.rewind_points[0].prompt_index, 3);
        assert_eq!(
            r.rewind_points[0].prompt_preview.as_deref(),
            Some("fix auth")
        );
    }

    #[test]
    fn recap_params_use_auto_not_max_turns() {
        let p = RecapParams {
            session_id: "s1".into(),
            auto: false,
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["sessionId"], "s1");
        assert_eq!(v["auto"], false);
        assert!(v.get("maxTurns").is_none());
    }

    #[test]
    fn session_bootstrap_result_reads_model() {
        let r: SessionBootstrapResult = serde_json::from_value(json!({
            "sessionId": "sess-1",
            "models": { "currentModelId": "grok-4" }
        }))
        .unwrap();
        assert_eq!(r.session_id.as_deref(), Some("sess-1"));
        assert_eq!(
            r.models
                .as_ref()
                .and_then(|m| m.current_model_id.as_deref()),
            Some("grok-4")
        );
        assert_eq!(r.resolve_session_id(None).as_deref(), Some("sess-1"));
    }

    #[test]
    fn session_bootstrap_result_reads_available_models() {
        let r: SessionBootstrapResult = serde_json::from_value(json!({
            "sessionId": "sess-1",
            "models": {
                "currentModelId": "grok-4.5",
                "availableModels": [
                    {
                        "modelId": "grok-4.5",
                        "name": "Grok 4.5",
                        "_meta": { "reasoningEffort": "high" }
                    },
                    { "modelId": "grok-4", "name": "Grok 4" }
                ]
            }
        }))
        .unwrap();
        let models = r.models.as_ref().expect("models");
        assert_eq!(models.current_model_id.as_deref(), Some("grok-4.5"));
        assert_eq!(models.available_models.len(), 2);
        assert_eq!(models.available_models[0].model_id, "grok-4.5");
        assert_eq!(models.available_models[0].name.as_deref(), Some("Grok 4.5"));
        assert_eq!(
            models.available_models[0]
                .meta
                .as_ref()
                .and_then(|meta| meta.get("reasoningEffort"))
                .and_then(serde_json::Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn session_models_info_deserializes_models_update_params() {
        // Wire shape of `x.ai/models/update` / `_x.ai/models/update` params.
        let m: SessionModelsInfo = serde_json::from_value(json!({
            "currentModelId": "grok-new",
            "availableModels": [
                { "modelId": "grok-new", "name": "Grok New" }
            ]
        }))
        .unwrap();
        assert_eq!(m.current_model_id.as_deref(), Some("grok-new"));
        assert_eq!(m.available_models[0].model_id, "grok-new");
    }

    #[test]
    fn session_load_response_without_session_id_deserializes() {
        // Mirrors Grok ACP `LoadSessionResponse::new()` — no sessionId field.
        let r: SessionBootstrapResult = serde_json::from_value(json!({
            "models": { "currentModelId": "grok-4" }
        }))
        .unwrap();
        assert!(r.session_id.is_none());
        assert_eq!(
            r.models
                .as_ref()
                .and_then(|m| m.current_model_id.as_deref()),
            Some("grok-4")
        );
        assert_eq!(
            r.resolve_session_id(Some("known-sess")).as_deref(),
            Some("known-sess")
        );
    }

    #[test]
    fn session_load_response_reads_running_prompt_fallback() {
        let r: SessionBootstrapResult = serde_json::from_value(json!({
            "_meta": { "x.ai/runningPromptId": "prompt-live" }
        }))
        .unwrap();
        assert_eq!(r.running_prompt_id(), Some("prompt-live"));
    }

    #[test]
    fn handshake_session_new_and_kill() {
        if std::env::var("CI").is_ok() {
            eprintln!("skip: CI environment detected");
            return;
        }
        let grok = std::env::var("GROK_BIN")
            .ok()
            .filter(|p| std::path::Path::new(p).is_file())
            .or_else(|| {
                std::env::var("GROK_HOME").ok().and_then(|h| {
                    let base = std::path::PathBuf::from(h).join("bin").join("grok");
                    #[cfg(windows)]
                    {
                        let exe = base.with_extension("exe");
                        if exe.is_file() {
                            return Some(exe.display().to_string());
                        }
                    }
                    base.is_file().then(|| base.display().to_string())
                })
            })
            .or_else(|| {
                let home = dirs::home_dir()?;
                let base = home.join(".grok").join("bin").join("grok");
                #[cfg(windows)]
                {
                    let exe = base.with_extension("exe");
                    if exe.is_file() {
                        return Some(exe.display().to_string());
                    }
                }
                base.is_file().then(|| base.display().to_string())
            });
        let Some(grok) = grok else {
            eprintln!("skip: grok not installed");
            return;
        };
        let cwd = std::env::temp_dir();
        let notifs = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&notifs);
        let client = AcpClient::spawn_with_notify(
            &grok,
            true,
            &[],
            &[],
            Arc::new(move |_m| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .expect("spawn");
        client.initialize().expect("init");
        let res = client
            .session_new(&cwd.display().to_string())
            .expect("session/new");
        assert!(res
            .session_id
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false));
        client.kill().expect("kill");
        assert!(notifs.load(Ordering::SeqCst) > 0);
    }
}
