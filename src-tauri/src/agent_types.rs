use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ManagedStatus {
    Starting,
    Ready,
    Running,
    AwaitingPermission,
    Stopping,
    Error,
    Stopped,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    #[default]
    Default,
    AcceptEdits,
    /// Grok Auto: host allows safe tools; ask on high risk.
    /// Spawn/attach pass top-level `grok --permission-mode auto agent --no-leader stdio`
    /// (`--permission-mode` is not under `agent` — clap rejects that and the
    /// process dies immediately).
    /// Live Mode toggles notify Grok via `x.ai/yolo_mode_changed`; the host
    /// never overrides a permission request that Grok's classifier escalated.
    Auto,
    BypassPermissions,
    DontAsk,
}

impl PermissionMode {
    pub fn spawns_always_approve(self) -> bool {
        matches!(self, Self::BypassPermissions)
    }

    /// Top-level `grok` flags that must appear **before** the `agent` subcommand.
    ///
    /// `--permission-mode` is defined on the root CLI (`grok --help`), not on
    /// `grok agent`. Wrong placement → immediate exit → "ACP transport closed".
    pub fn spawn_global_args(self) -> Vec<String> {
        match self {
            Self::Auto => vec!["--permission-mode".into(), "auto".into()],
            _ => Vec::new(),
        }
    }

    /// Extra CLI args after `grok agent` (before `stdio`).
    pub fn spawn_agent_args(self) -> Vec<String> {
        Vec::new()
    }

    pub fn from_request(mode: Option<Self>, always_approve: Option<bool>) -> Self {
        mode.unwrap_or_else(|| {
            if always_approve == Some(true) {
                Self::BypassPermissions
            } else {
                Self::Default
            }
        })
    }
}

/// Model advertised by the agent (session/new|load `models` or `x.ai/models/update`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModelInfo {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the model accepts a reasoning level, as advertised by Grok ACP.
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    /// Catalog-default reasoning level advertised by Grok ACP. Session choice
    /// lives on [`ManagedAgentInfo::reasoning_effort`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Model-specific reasoning levels advertised by Grok ACP.
    #[serde(default)]
    pub reasoning_efforts: Vec<ReasoningEffortOption>,
}

/// One selectable reasoning level from a model's ACP metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortOption {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedAgentInfo {
    pub handle_id: String,
    pub session_id: Option<String>,
    pub cwd: String,
    pub pid: Option<u32>,
    pub status: ManagedStatus,
    pub permission_mode: PermissionMode,
    pub always_approve: bool,
    pub model_id: Option<String>,
    /// Session reasoning level. Catalog refresh must not overwrite this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Live catalog from the agent. Empty until bootstrap/models/update fills it.
    #[serde(default)]
    pub available_models: Vec<AvailableModelInfo>,
    pub last_error: Option<String>,
    pub title: Option<String>,
    pub created_at: String,
    pub pending_permission_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnRequest {
    pub cwd: String,
    pub prompt: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub always_approve: Option<bool>,
    pub model: Option<String>,
    /// ACP session mode id to apply after `session/new` (e.g. `"plan"`).
    /// Independent of host permission mode. Empty/None → leave agent default.
    #[serde(default)]
    pub session_mode_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachRequest {
    pub session_id: String,
    pub cwd: String,
    pub permission_mode: Option<PermissionMode>,
    pub always_approve: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PermissionKind {
    ToolPermission,
    FsWrite,
    FsRead,
    /// Grok `x.ai/exit_plan_mode` reverse-RPC — host shows plan preview.
    PlanApproval,
    /// Grok `x.ai/ask_user_question` reverse-RPC — multi-choice Q&A form.
    UserQuestion,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPermission {
    pub request_key: String,
    pub handle_id: String,
    pub session_id: Option<String>,
    pub request_id: Value,
    pub kind: PermissionKind,
    pub method: String,
    pub title: String,
    pub detail: String,
    pub risk: String,
    pub options: Vec<PermissionOption>,
    pub raw_params: Value,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvePermissionRequest {
    pub handle_id: String,
    pub request_key: String,
    pub option_id: String,
    /// Freeform notes for plan approval (approve-with-comments / request-changes).
    #[serde(default)]
    pub comments: Option<String>,
    /// Structured payload for ask-user answers (and future rich resolves).
    #[serde(default)]
    pub payload: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_always_approve_maps_to_bypass() {
        assert_eq!(
            PermissionMode::from_request(None, Some(true)),
            PermissionMode::BypassPermissions
        );
        assert_eq!(
            PermissionMode::from_request(Some(PermissionMode::AcceptEdits), Some(true)),
            PermissionMode::AcceptEdits
        );
    }

    #[test]
    fn auto_mode_uses_top_level_permission_flag() {
        assert_eq!(
            PermissionMode::Auto.spawn_global_args(),
            vec!["--permission-mode", "auto"]
        );
        assert!(PermissionMode::Auto.spawn_agent_args().is_empty());
        assert!(PermissionMode::Default.spawn_global_args().is_empty());
        assert!(PermissionMode::BypassPermissions
            .spawn_global_args()
            .is_empty());
    }

    #[test]
    fn serialized_enums_match_shared_contract() {
        let contract: Value =
            serde_json::from_str(include_str!("../../contracts/agent-contract.json"))
                .expect("contract json");
        let statuses = [
            ManagedStatus::Starting,
            ManagedStatus::Ready,
            ManagedStatus::Running,
            ManagedStatus::AwaitingPermission,
            ManagedStatus::Stopping,
            ManagedStatus::Error,
            ManagedStatus::Stopped,
        ];
        let modes = [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::BypassPermissions,
            PermissionMode::DontAsk,
        ];
        let kinds = [
            PermissionKind::ToolPermission,
            PermissionKind::FsWrite,
            PermissionKind::FsRead,
            PermissionKind::PlanApproval,
            PermissionKind::UserQuestion,
            PermissionKind::Other,
        ];
        assert_eq!(json!(statuses), contract["managedStatuses"]);
        assert_eq!(json!(modes), contract["permissionModes"]);
        assert_eq!(json!(kinds), contract["permissionKinds"]);
    }
}
