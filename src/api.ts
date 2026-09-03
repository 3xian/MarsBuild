import { invoke } from "@tauri-apps/api/core";
import type {
  ActiveSession,
  AvailableCommand,
  AttachRequest,
  CancelSubagentResult,
  DashboardStats,
  DirEntry,
  FilePreview,
  GitBranchInfo,
  GitChange,
  GitFileDiff,
  HunkPage,
  KillTaskResult,
  LiveSessionUsage,
  ListSubagentsResult,
  ListTasksResult,
  ManagedAgentInfo,
  PendingPermission,
  PermissionMode,
  PromptQueueEntry,
  SessionCard,
  SessionDetail,
  SessionRecapResult,
  SessionTokenUsageInfo,
  SessionUpdatePage,
  SpawnRequest,
  TokenUsageSeries,
  WeekUsage,
} from "./types";

export async function getGrokHome(): Promise<string> {
  return invoke<string>("get_grok_home");
}

export async function listActiveSessions(): Promise<ActiveSession[]> {
  return invoke<ActiveSession[]>("list_active_sessions");
}

export async function listSessions(limit?: number): Promise<SessionCard[]> {
  return invoke<SessionCard[]>("list_sessions", { limit: limit ?? null });
}

export async function searchSessions(
  query: string,
  limit = 100,
): Promise<SessionCard[]> {
  return invoke<SessionCard[]>("search_sessions", { query, limit });
}

export async function getSessionCard(sessionId: string): Promise<SessionCard> {
  return invoke<SessionCard>("get_session_card", { sessionId });
}

export async function listSessionTokenUsages(
  sessionIds: string[],
): Promise<SessionTokenUsageInfo[]> {
  return invoke<SessionTokenUsageInfo[]>("list_session_token_usages", {
    sessionIds,
  });
}

export async function getSessionDetail(sessionId: string): Promise<SessionDetail> {
  return invoke<SessionDetail>("get_session_detail", { sessionId });
}

export async function listSessionUpdates(
  sessionId: string,
  beforeCursor: number,
  limit?: number,
): Promise<SessionUpdatePage> {
  return invoke<SessionUpdatePage>("list_session_updates", {
    sessionId,
    beforeCursor,
    limit: limit ?? null,
  });
}

export async function listSessionHunks(
  sessionId: string,
  limit?: number,
): Promise<HunkPage> {
  return invoke<HunkPage>("list_session_hunks", {
    sessionId,
    limit: limit ?? null,
  });
}

export async function getDashboardStats(): Promise<DashboardStats> {
  return invoke<DashboardStats>("get_dashboard_stats");
}

export async function getTokenUsageSeries(days = 7): Promise<TokenUsageSeries> {
  return invoke<TokenUsageSeries>("get_token_usage_series", { days });
}

export async function getWeekUsage(): Promise<WeekUsage> {
  return invoke<WeekUsage>("get_week_usage");
}

export async function resolveGrokBin(): Promise<string> {
  return invoke<string>("resolve_grok_bin");
}

export async function listSlashSkills(cwd: string): Promise<AvailableCommand[]> {
  return invoke<AvailableCommand[]>("list_slash_skills", { cwd });
}

export async function listManagedAgents(): Promise<ManagedAgentInfo[]> {
  return invoke<ManagedAgentInfo[]>("list_managed_agents");
}

export async function spawnAgent(request: SpawnRequest): Promise<ManagedAgentInfo> {
  return invoke<ManagedAgentInfo>("spawn_agent", { request });
}

export async function attachAgent(request: AttachRequest): Promise<ManagedAgentInfo> {
  return invoke<ManagedAgentInfo>("attach_agent", { request });
}

/** Tauri `invoke` rejects with a string, Error, or `{ message }`. */
export function formatInvokeError(error: unknown): string {
  if (typeof error === "string" && error.trim()) return error;
  if (error instanceof Error && error.message.trim()) return error.message;
  if (error && typeof error === "object") {
    const rec = error as { message?: unknown; error?: unknown };
    if (typeof rec.message === "string" && rec.message.trim()) {
      return rec.message;
    }
    if (typeof rec.error === "string" && rec.error.trim()) {
      return rec.error;
    }
  }
  return String(error);
}

const EXCLUSIVE_SESSION_CODE = "session_open_elsewhere";

function invokeErrorCode(error: unknown): string | undefined {
  if (!error || typeof error !== "object") return undefined;
  const rec = error as { code?: unknown; error?: unknown };
  if (typeof rec.code === "string" && rec.code.trim()) return rec.code;
  if (rec.error && typeof rec.error === "object") {
    const inner = rec.error as { code?: unknown };
    if (typeof inner.code === "string" && inner.code.trim()) return inner.code;
  }
  return undefined;
}

/** Attach refused because another Grok pid owns the session. */
export function isExclusiveSessionError(error: unknown): boolean {
  return invokeErrorCode(error) === EXCLUSIVE_SESSION_CODE;
}

export async function promptAgent(
  handleId: string,
  text: string,
): Promise<{
  accepted: boolean;
  handleId: string;
  sessionId: string;
  /** Managed agent status after accept (usually `"running"`). */
  status?: ManagedAgentInfo["status"];
}> {
  return invoke("prompt_agent", { handleId, text });
}

export async function interjectAgent(
  handleId: string,
  text: string,
): Promise<{ status?: string }> {
  return invoke("interject_agent", { handleId, text });
}

export async function removeQueuedPrompt(
  handleId: string,
  entry: Pick<PromptQueueEntry, "id" | "version">,
): Promise<void> {
  return invoke("queue_remove", {
    handleId,
    id: entry.id,
    version: entry.version,
  });
}

export async function reorderQueuedPrompts(
  handleId: string,
  orderedIds: string[],
): Promise<void> {
  return invoke("queue_reorder", { handleId, orderedIds });
}

export async function clearQueuedPrompts(handleId: string): Promise<void> {
  return invoke("queue_clear", { handleId });
}

export async function editQueuedPrompt(
  handleId: string,
  id: string,
  newText: string,
): Promise<void> {
  return invoke("queue_edit", { handleId, id, newText });
}

export async function interjectQueuedPrompt(
  handleId: string,
  entry: Pick<PromptQueueEntry, "id" | "version">,
): Promise<void> {
  return invoke("queue_interject", {
    handleId,
    id: entry.id,
    version: entry.version,
  });
}

export async function stopAgent(handleId: string): Promise<ManagedAgentInfo> {
  return invoke<ManagedAgentInfo>("stop_agent", { handleId });
}

export async function findManagedBySession(
  sessionId: string,
): Promise<ManagedAgentInfo | null> {
  return invoke<ManagedAgentInfo | null>("find_managed_by_session", { sessionId });
}

export async function listPendingPermissions(
  handleId?: string | null,
): Promise<PendingPermission[]> {
  return invoke<PendingPermission[]>("list_pending_permissions", {
    handleId: handleId ?? null,
  });
}

export async function resolvePermission(
  handleId: string,
  requestKey: string,
  optionId: string,
  comments?: string | null,
  /** Structured answers for userQuestion (see UserQuestionResolvePayload). */
  payload?: object | null,
): Promise<PendingPermission> {
  return invoke<PendingPermission>("resolve_permission", {
    request: {
      handleId,
      requestKey,
      optionId,
      comments: comments ?? null,
      payload: payload ?? null,
    },
  });
}

export async function setPermissionMode(
  handleId: string,
  mode: PermissionMode,
): Promise<ManagedAgentInfo> {
  return invoke<ManagedAgentInfo>("set_permission_mode", {
    handleId,
    mode,
  });
}

/**
 * ACP `session/set_mode` — e.g. `"plan"` / `"default"`.
 * Prefixing `/plan` in prompt text alone does not enter Grok plan mode over ACP.
 */
export async function setSessionMode(
  handleId: string,
  modeId: string,
): Promise<void> {
  return invoke("set_session_mode", { handleId, modeId });
}

/** Persist mode for a session even when no agent is live. */
export async function setTaskPermissionMode(
  sessionId: string,
  mode: PermissionMode,
): Promise<void> {
  return invoke("set_task_permission_mode", { sessionId, mode });
}

export async function getTaskPermissionMode(
  sessionId: string,
): Promise<PermissionMode | null> {
  return invoke<PermissionMode | null>("get_task_permission_mode", {
    sessionId,
  });
}

export async function listTaskPermissionModes(): Promise<
  Record<string, PermissionMode>
> {
  return invoke<Record<string, PermissionMode>>("list_task_permission_modes");
}

/** New Task default mode. Pass `projectCwd` so project-layer config can apply. */
export async function getLastSpawnPermissionMode(
  projectCwd?: string | null,
): Promise<PermissionMode> {
  return invoke<PermissionMode>("get_last_spawn_permission_mode", {
    projectCwd: projectCwd?.trim() || null,
  });
}

/** Persist Grok Plan arming (Pending) for a session. */
export async function setTaskPlanArmed(
  sessionId: string,
  armed: boolean,
): Promise<void> {
  return invoke("set_task_plan_armed", { sessionId, armed });
}

export async function getTaskPlanArmed(sessionId: string): Promise<boolean> {
  return invoke<boolean>("get_task_plan_armed", { sessionId });
}

export async function listTaskPlanArmed(): Promise<Record<string, boolean>> {
  return invoke<Record<string, boolean>>("list_task_plan_armed");
}

/** Session plan.md (Grok plan mode artifact under ~/.grok/sessions/…). */
export interface SessionPlan {
  path: string;
  content: string;
  empty: boolean;
}

export async function getSessionPlan(
  sessionId: string,
): Promise<SessionPlan | null> {
  return invoke<SessionPlan | null>("get_session_plan", { sessionId });
}

export async function listProjectDir(
  root: string,
  path?: string | null,
): Promise<DirEntry[]> {
  return invoke<DirEntry[]>("list_project_dir", {
    root,
    path: path ?? null,
  });
}

/** Open a project path with the OS default application (must stay under root). */
export async function openProjectPath(
  root: string,
  path: string,
  sessionId?: string | null,
): Promise<void> {
  return invoke("open_project_path", {
    root,
    path,
    sessionId: sessionId ?? null,
  });
}

/**
 * Read a project file (or Grok session asset such as `images/1.jpg`) for the
 * in-app preview pane.
 */
export async function readProjectFile(
  root: string,
  path: string,
  sessionId?: string | null,
): Promise<FilePreview> {
  return invoke<FilePreview>("read_project_file", {
    root,
    path,
    sessionId: sessionId ?? null,
  });
}

export async function gitStatus(cwd: string): Promise<GitChange[]> {
  return invoke<GitChange[]>("git_status", { cwd });
}

export async function getSessionUsage(
  handleId: string,
): Promise<LiveSessionUsage> {
  return invoke<LiveSessionUsage>("get_session_usage", { handleId });
}

/** ACP `session/set_model` — switch model during session. */
export async function setSessionModel(
  handleId: string,
  modelId: string,
  reasoningEffort?: string | null,
): Promise<Record<string, unknown>> {
  return invoke("set_session_model", {
    handleId,
    modelId,
    reasoningEffort: reasoningEffort ?? null,
  });
}

/**
 * ACP `x.ai/recap` — request a recap (fire-and-forget).
 * Summary text arrives later as a timeline `session_recap` event.
 */
export async function getSessionRecap(
  handleId: string,
  auto = false,
): Promise<SessionRecapResult> {
  return invoke<SessionRecapResult>("get_session_recap", {
    handleId,
    auto,
  });
}

/** ACP `x.ai/rewind/points` — list rewindable points. */
export async function getRewindPoints(
  handleId: string,
): Promise<Record<string, unknown>> {
  return invoke("get_rewind_points", { handleId });
}

/** ACP `x.ai/rewind/execute` — rewind to target prompt index. */
export async function rewindExecute(
  handleId: string,
  targetPromptIndex: number,
  mode?: string | null,
): Promise<Record<string, unknown>> {
  return invoke("rewind_execute", {
    handleId,
    targetPromptIndex,
    mode: mode ?? null,
  });
}

/** ACP `x.ai/subagent/cancel` — cancel running subagent. */
export async function cancelSubagent(
  handleId: string,
  subagentId: string,
): Promise<CancelSubagentResult> {
  return invoke<CancelSubagentResult>("cancel_subagent", {
    handleId,
    subagentId,
  });
}

/** ACP `x.ai/subagent/list_running` — list running subagents. */
export async function listSubagents(
  handleId: string,
): Promise<ListSubagentsResult> {
  return invoke<ListSubagentsResult>("list_subagents", { handleId });
}

/** ACP `x.ai/task/kill` — kill a background task. */
export async function killTask(
  handleId: string,
  taskId: string,
): Promise<KillTaskResult> {
  return invoke<KillTaskResult>("kill_task", { handleId, taskId });
}

/** ACP `x.ai/task/list` — list background tasks. */
export async function listTasks(handleId: string): Promise<ListTasksResult> {
  return invoke<ListTasksResult>("list_tasks", { handleId });
}

/** Git branch info: name, upstream, ahead/behind, staged/unstaged counts. */
export async function gitBranchInfo(cwd: string): Promise<GitBranchInfo> {
  return invoke<GitBranchInfo>("git_branch_info", { cwd });
}

/** Git unified diff for a single file. */
export async function gitDiffFile(
  cwd: string,
  path: string,
  staged: boolean,
): Promise<GitFileDiff> {
  return invoke<GitFileDiff>("git_diff_file", { cwd, path, staged });
}

/** Stage a file (git add). */
export async function gitStageFile(cwd: string, path: string): Promise<void> {
  return invoke("git_stage_file", { cwd, path });
}

/** Unstage a file (git reset HEAD). */
export async function gitUnstageFile(cwd: string, path: string): Promise<void> {
  return invoke("git_unstage_file", { cwd, path });
}

/** Stage all changes. */
export async function gitStageAll(cwd: string): Promise<void> {
  return invoke("git_stage_all", { cwd });
}

/** Unstage all changes. */
export async function gitUnstageAll(cwd: string): Promise<void> {
  return invoke("git_unstage_all", { cwd });
}

/** Commit staged changes. */
export async function gitCommit(
  cwd: string,
  message: string,
): Promise<string> {
  return invoke<string>("git_commit", { cwd, message });
}

/**
 * Apply a unified-diff patch to the index (`git apply --cached`).
 * Pass `reverse: true` to unstage the selected hunks.
 */
export async function gitApplyPatch(
  cwd: string,
  patch: string,
  reverse = false,
): Promise<void> {
  return invoke("git_apply_patch", { cwd, patch, reverse });
}
