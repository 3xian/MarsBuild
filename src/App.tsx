import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  attachAgent,
  formatInvokeError,
  isExclusiveSessionError,
  getLastSpawnPermissionMode,
  getSessionCard,
  getSessionDetail,
  interjectAgent,
  listManagedAgents,
  listPendingPermissions,
  listTaskPermissionModes,
  listTaskPlanArmed,
  promptAgent,
  resolvePermission,
  setPermissionMode,
  setSessionMode,
  setTaskPermissionMode,
  setTaskPlanArmed,
  spawnAgent,
  stopAgent,
} from "./api";
import { MacosTitlebarBrand } from "./components/MacosTitlebarBrand";
import type { SendResult } from "./components/PromptBar";
import { NewTaskModal } from "./components/NewTaskModal";
import { SessionDetailView } from "./components/SessionDetail";
import { SessionList } from "./components/SessionList";
import { StatsBar } from "./components/StatsBar";
import { UpdateModal } from "./components/UpdateModal";
import { WindowsTitlebar } from "./components/WindowsTitlebar";
import { WorkspacePanel } from "./components/WorkspacePanel";
import { useAgentEvents } from "./hooks/useAgentEvents";
import { useAppUpdate } from "./hooks/useAppUpdate";
import { usePromptQueueController } from "./hooks/usePromptQueueController";
import { useSessionModel } from "./hooks/useSessionModel";
import { useSessionPlanMode } from "./hooks/useSessionPlanMode";
import { useSessionIndex } from "./hooks/useSessionIndex";
import { useSlashCommandCatalog } from "./hooks/useSlashCommandCatalog";
import { useTimelineHistory } from "./hooks/useTimelineHistory";
import { useUsageMetrics } from "./hooks/useUsageMetrics";
import type {
  MainTab,
  ManagedAgentInfo,
  PendingPermission,
  PermissionMode,
  SessionCard,
  SessionDetail,
  SessionMode,
} from "./types";
import {
  isLocalSlashCommand,
  runLocalSlash,
} from "./utils/localSlash";
import {
  isAttachedManagedStatus,
  isLiveManagedStatus,
} from "./utils/managedStatus";
import { SEND_REFUSAL_HINT } from "./utils/turnActivity";
import { pickSelectedId } from "./utils/pickSelectedId";
import type { UserQuestionResolvePayload } from "./utils/permissionPayload";
import { joinUnderRoot } from "./utils/paths";
import {
  applySessionModeChange,
  applySessionModeToPrompt,
  displaySessionMode,
  sessionModeFromPermission,
  sessionModeWireId,
} from "./utils/sessionMode";
import { displayedSessionModel } from "./utils/sessionModel";
import "./App.css";

/**
 * Sole debounce for disk-driven UI refresh (session list, detail, workspace
 * FileTree / GitChanges via gitRefreshKey). Children do not re-debounce.
 */
const FS_REFRESH_MIN_MS = 400;
/** Slow safety net if FSEvents miss a write (rare). */
const SAFETY_POLL_MS = 90_000;

function App() {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const selectedIdRef = useRef(selectedId);
  selectedIdRef.current = selectedId;
  const [detail, setDetail] = useState<SessionDetail | null>(null);
  const [tab, setTab] = useState<MainTab>("timeline");
  const [detailLoading, setDetailLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [modalOpen, setModalOpen] = useState(false);
  /** Pending Stop confirmation: which managed agent to kill. */
  const [stopConfirm, setStopConfirm] = useState<{
    handleId: string;
    sessionId: string;
    title: string;
  } | null>(null);
  /** Bumped after attach/spawn so Timeline pins to bottom. */
  const [pinTimelineBottomSeq, setPinTimelineBottomSeq] = useState(0);
  const [controlBusy, setControlBusy] = useState(false);
  const [permBusyKey, setPermBusyKey] = useState<string | null>(null);
  /** Per-task permission modes loaded from disk (`~/.pinkcode/task_prefs.json`). */
  const [taskPermissionModes, setTaskPermissionModes] = useState<
    Record<string, PermissionMode>
  >({});
  /** Seed for New Task modal Mode selector; last session mode used when spawning. */
  const [lastSpawnSessionMode, setLastSpawnSessionMode] =
    useState<SessionMode>("normal");

  /** Bump git changes panel after disk events. */
  const [gitRefreshKey, setGitRefreshKey] = useState(0);
  /**
   * File preview selection — always absolute under project root when set
   * (see openPreview). Relative paths from markdown/tools are normalized here.
   */
  const [previewPath, setPreviewPath] = useState<string | null>(null);
  /** Right workspace rail collapsed (Ctrl+H). */
  const [workspaceCollapsed, setWorkspaceCollapsed] = useState(false);

  const planMode = useSessionPlanMode();
  const sessionModel = useSessionModel();
  const {
    managedList,
    managedForSession,
    timelineItems,
    availableCommands,
    permissionsForSession,
    lastError,
    clearError,
    upsertManaged,
    removeManaged,
    removePermission,
    hydratePermissions,
    appendLocalLive,
    hydrateDiskLive,
    cancelSubagent,
    killTask,
    needsInputSessionIds,
  } = useAgentEvents(selectedId, {
    onCurrentModeUpdate: planMode.onAgentModeUpdate,
  });
  const onRecentSessionsLoaded = useCallback(
    async (list: SessionCard[]) => {
      const previous = selectedIdRef.current;
      let prevOnDisk: boolean | undefined;
      if (previous && !list.some((s) => s.id === previous)) {
        try {
          await getSessionCard(previous);
          prevOnDisk = true;
        } catch {
          prevOnDisk = false;
        }
      }
      setSelectedId((prev) => pickSelectedId(list, prev, { prevOnDisk }));
      try {
        const managed = await listManagedAgents();
        for (const item of managed) {
          upsertManaged(item);
        }
        setSelectedId((prev) =>
          pickSelectedId(list, prev, { prevOnDisk, managed }),
        );
      } catch {
        /* managed agents are optional during startup */
      }
    },
    [upsertManaged],
  );
  const {
    sessions,
    query,
    setQuery,
    refreshList,
    refreshCard,
    mergeCard: mergeSessionCard,
    hasMore: hasMoreSessions,
    loadMore: loadMoreSessions,
  } = useSessionIndex({
    selectedId,
    onRecentLoaded: onRecentSessionsLoaded,
    onError: setError,
  });
  // ACP owns the live tail when attached; disk-only sessions re-hydrate on poll.
  // Include `starting` so reconnect does not replace the open pane with page 1.
  const liveOwnsTail =
    managedForSession != null &&
    isAttachedManagedStatus(managedForSession.status);
  const liveOwnsTailRef = useRef(liveOwnsTail);
  liveOwnsTailRef.current = liveOwnsTail;
  const timelineHistory = useTimelineHistory(
    selectedId,
    detail,
    hydrateDiskLive,
    liveOwnsTail,
  );
  const promptQueue = usePromptQueueController(
    selectedId,
    managedForSession,
    setError,
  );

  const projectCwd = detail?.card.cwd ?? null;
  useEffect(() => {
    // New project → clear previous preview selection.
    setPreviewPath(null);
  }, [projectCwd]);
  const promptCommands = useSlashCommandCatalog(projectCwd, availableCommands);

  /** Single write boundary for preview: one path identity (absolute under root). */
  const openPreview = useCallback(
    (path: string | null) => {
      if (!path) {
        setPreviewPath(null);
        return;
      }
      const root = projectCwd;
      setPreviewPath(root ? joinUnderRoot(root, path) : path);
    },
    [projectCwd],
  );
  const {
    pendingUpdate,
    dismissUpdate,
    checkForUpdate,
    updateCheckStatus,
  } = useAppUpdate();

  // Hydrate per-task permission modes, Plan arming, + last spawn seed on mount.
  useEffect(() => {
    void (async () => {
      try {
        const [modes, planArmed, last] = await Promise.all([
          listTaskPermissionModes(),
          listTaskPlanArmed(),
          getLastSpawnPermissionMode(),
        ]);
        setTaskPermissionModes(modes);
        planMode.hydrate(planArmed);
        setLastSpawnSessionMode(sessionModeFromPermission(last));
      } catch {
        /* non-tauri / first run */
      }
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- mount-only hydrate
  }, []);

  /** Mode shown/edited for the selected task. Attached agent wins when present. */
  const effectivePermissionMode: PermissionMode = useMemo(() => {
    if (
      managedForSession &&
      managedForSession.status !== "stopped" &&
      managedForSession.status !== "error"
    ) {
      return managedForSession.permissionMode;
    }
    if (selectedId && taskPermissionModes[selectedId]) {
      return taskPermissionModes[selectedId];
    }
    return "default";
  }, [managedForSession, selectedId, taskPermissionModes]);

  /** Single Mode chip: planArmed + host permission (no second Mode map). */
  const planArmedSelected = planMode.isArmed(selectedId);
  const effectiveSessionMode: SessionMode = useMemo(
    () => displaySessionMode(planArmedSelected, effectivePermissionMode),
    [planArmedSelected, effectivePermissionMode],
  );

  const detailReqSeq = useRef(0);
  const lastFsRefreshRef = useRef(0);
  /** Session id we intentionally focused (spawn); ignore auto-steal otherwise. */
  const focusOnceSessionRef = useRef<string | null>(null);

  const refreshDetail = useCallback(
    async (id: string, silent = false) => {
      const seq = ++detailReqSeq.current;
      if (!silent) setDetailLoading(true);
      try {
        const d = await getSessionDetail(id);
        // Ignore stale responses if the user switched sessions mid-flight.
        if (seq !== detailReqSeq.current || selectedIdRef.current !== id) {
          return;
        }
        setDetail(d);
        mergeSessionCard(d.card);
        setDetailError(null);
      } catch (e) {
        if (seq !== detailReqSeq.current || selectedIdRef.current !== id) {
          return;
        }
        setDetailError(e instanceof Error ? e.message : String(e));
      } finally {
        if (!silent && seq === detailReqSeq.current) {
          setDetailLoading(false);
        }
      }
    },
    [mergeSessionCard],
  );

  const refreshFromDisk = useCallback(() => {
    const now = Date.now();
    if (now - lastFsRefreshRef.current < FS_REFRESH_MIN_MS) return;
    lastFsRefreshRef.current = now;
    void refreshList();
    const id = selectedIdRef.current;
    // ACP owns the tail (starting included). Disk-only panes still need
    // silent getSessionDetail so updates.jsonl refreshes the open timeline.
    if (id && !liveOwnsTailRef.current) void refreshDetail(id, true);
    setGitRefreshKey((n) => n + 1);
  }, [refreshList, refreshDetail]);

  const liveManagedCount = useMemo(
    () =>
      managedList.filter(
        (m) => m.status !== "stopped" && m.status !== "error",
      ).length,
    [managedList],
  );

  const { tokenSeries, weekUsage, refreshWeekUsage } = useUsageMetrics(
    liveManagedCount,
    () => setGitRefreshKey((n) => n + 1),
  );

  const loadedDetailIdRef = useRef<string | null>(null);
  useEffect(() => {
    if (!selectedId) {
      setDetail(null);
      loadedDetailIdRef.current = null;
      return;
    }
    // Load once per selected task. refreshDetail identity must not replay
    // getSessionDetail (first history page) into a live pane.
    if (loadedDetailIdRef.current === selectedId) return;
    loadedDetailIdRef.current = selectedId;
    void refreshDetail(selectedId);
  }, [selectedId, refreshDetail]);

  // Primary: debounced FS watcher on ~/.grok/sessions + active_sessions.json
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void listen<{
      reason?: string;
      category?: "index" | "timeline" | "hunks" | "plan";
      sessionId?: string | null;
      path?: string;
    }>("sessions-changed", ({ payload }) => {
      if (cancelled || document.visibilityState === "hidden") return;
      const selected = selectedIdRef.current;
      if (payload.category === "index") {
        if (payload.sessionId) void refreshCard(payload.sessionId);
        else void refreshList();
      } else if (
        payload.category === "timeline" &&
        payload.sessionId &&
        payload.sessionId !== selected
      ) {
        // Marks token usage pending and lets the background hydrator scan only
        // the appended bytes for this card.
        void refreshCard(payload.sessionId);
      }
      if (
        selected &&
        !liveOwnsTailRef.current &&
        (!payload.sessionId || payload.sessionId === selected)
      ) {
        void refreshDetail(selected, true);
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [refreshList, refreshCard, refreshDetail]);

  // These are advertised ACP capabilities, so consume their notifications
  // and invalidate the workspace immediately.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    void listen<{
      method?: string;
      sessionId?: string | null;
      params?: unknown;
    }>("agent-notification", ({ payload }) => {
      if (cancelled || document.visibilityState === "hidden") return;
      if (
        payload.method !== "x.ai/fs_notify" &&
        payload.method !== "x.ai/git_head_changed"
      ) {
        return;
      }
      const selected = selectedIdRef.current;
      if (payload.sessionId && selected && payload.sessionId !== selected) {
        return;
      }
      setGitRefreshKey((n) => n + 1);
      if (selected && !liveOwnsTailRef.current) {
        void refreshDetail(selected, true);
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [refreshDetail]);

  // Focus / tab visible → catch anything the watcher missed (debounced).
  useEffect(() => {
    let t: number | null = null;
    const onVis = () => {
      if (document.visibilityState !== "visible") return;
      if (t != null) window.clearTimeout(t);
      t = window.setTimeout(() => {
        t = null;
        refreshFromDisk();
      }, 300);
    };
    document.addEventListener("visibilitychange", onVis);
    // WebKitGTK/Wayland fires window focus on every click; that used to
    // refreshFromDisk → roster reload → selection bounce → page-1 remount.
    return () => {
      document.removeEventListener("visibilitychange", onVis);
      if (t != null) window.clearTimeout(t);
    };
  }, [refreshFromDisk]);

  // Slow safety net only (not the main update path)
  useEffect(() => {
    const t = window.setInterval(() => refreshFromDisk(), SAFETY_POLL_MS);
    return () => window.clearInterval(t);
  }, [refreshFromDisk]);

  // Only auto-focus a session we just spawned (never steal focus from other tasks).
  useEffect(() => {
    const want = focusOnceSessionRef.current;
    if (!want) return;
    const ready = managedList.find(
      (m) =>
        m.sessionId === want &&
        m.status !== "stopped" &&
        m.status !== "error",
    );
    if (ready?.sessionId) {
      setSelectedId(ready.sessionId);
      focusOnceSessionRef.current = null;
    }
  }, [managedList]);

  async function handleSpawn(opts: {
    cwd: string;
    prompt: string;
    sessionMode: SessionMode;
  }) {
    setControlBusy(true);
    setError(null);
    try {
      // Map UI Mode → host gate + Plan arming (same rules as the composer chip).
      const next = applySessionModeChange(opts.sessionMode, "default");
      const permissionMode = next.permission ?? "default";
      const rawPrompt = opts.prompt.trim();
      const prompt = rawPrompt
        ? applySessionModeToPrompt(opts.sessionMode, rawPrompt)
        : null;

      const info = await spawnAgent({
        cwd: opts.cwd,
        // Empty / null → backend treats as “no initial prompt”.
        prompt,
        permissionMode,
        // ACP session mode (not host permission). Applied before initial prompt.
        sessionModeId: next.planArmed ? "plan" : null,
      });
      // With an initial prompt the agent is already Running; paint that immediately.
      upsertManaged(
        prompt && info.status === "ready"
          ? { ...info, status: "running" }
          : info,
      );
      setLastSpawnSessionMode(opts.sessionMode);
      if (info.sessionId) {
        const sessionId = info.sessionId;
        setTaskPermissionModes((prev) => ({
          ...prev,
          [sessionId]: permissionMode,
        }));
        if (next.planArmed) {
          // Spawn already called session/set_mode("plan"); track Active.
          await planMode.applyAfterSpawn(sessionId);
        }
        focusOnceSessionRef.current = sessionId;
        setSelectedId(sessionId);
        setTab("timeline");
        setPinTimelineBottomSeq((n) => n + 1);
      } else if (info.status === "error") {
        // Failed after process start — still surface in managed list until Stop.
        setError(info.lastError ?? "Agent failed to start");
      }
      setModalOpen(false);
      // Disk index may lag a moment
      window.setTimeout(() => void refreshList(), 800);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setControlBusy(false);
    }
  }

  /**
   * Connect ACP for a session if needed. No list-side toggle — desktop apps
   * connect when the user chats (or New task spawns already connected).
   * Returns the live handle, or null on failure / missing card.
   */
  async function ensureAttached(
    sessionId: string,
  ): Promise<ManagedAgentInfo | null> {
    const existing = managedList.find(
      (m) => m.sessionId === sessionId && isAttachedManagedStatus(m.status),
    );
    if (existing) return existing;

    const card = sessions.find((s) => s.id === sessionId);
    if (!card) return null;

    setSelectedId(sessionId);
    // Backend restores this task's saved mode when permissionMode is omitted.
    const saved = taskPermissionModes[sessionId];
    let info: ManagedAgentInfo;
    try {
      info = await attachAgent({
        sessionId: card.id,
        cwd: card.cwd,
        permissionMode: saved ?? null,
      });
    } catch (e) {
      // Another Grok process owns the session. Open in Grok Build chrome
      // is the signal; do not paint the error banner.
      if (isExclusiveSessionError(e)) return null;
      throw e;
    }
    upsertManaged(info);
    if (info.sessionId) {
      setTaskPermissionModes((prev) => ({
        ...prev,
        [info.sessionId!]: info.permissionMode,
      }));
      // Re-apply local Plan Pending after attach (Grok session mode is not
      // restored by host prefs alone — call ACP set_mode when armed).
      await planMode.reapplyAfterAttach(info.handleId, info.sessionId);
    }
    setTab("timeline");
    setPinTimelineBottomSeq((n) => n + 1);
    const queued = await listPendingPermissions(info.handleId);
    hydratePermissions(queued);
    // Lifecycle list_running / task/list refill is owned by useAgentEvents.

    if (info.status === "error" || info.status === "stopped") {
      throw new Error(info.lastError ?? "Failed to connect agent");
    }
    return info;
  }



  async function handleResolvePermission(
    item: PendingPermission,
    optionId: string,
    comments?: string,
    payload?: UserQuestionResolvePayload,
  ) {
    setPermBusyKey(item.requestKey);
    setError(null);
    try {
      await resolvePermission(
        item.handleId,
        item.requestKey,
        optionId,
        comments,
        payload ?? null,
      );
      removePermission(item.requestKey);

      if (item.kind === "planApproval") {
        await planMode.onPlanApprovalResolved(
          item.sessionId ?? selectedId,
          optionId,
        );
        // The plan response has no feedback field for approval. Route review
        // notes into the active turn through Grok's interjection extension.
        if (optionId === "approve" && comments?.trim() && item.handleId) {
          try {
            await interjectAgent(
              item.handleId,
              `The user approved the plan with the following review comments:\n\n${comments.trim()}`,
            );
            setPinTimelineBottomSeq((n) => n + 1);
          } catch (e) {
            setError(e instanceof Error ? e.message : String(e));
          }
        }
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setPermBusyKey(null);
    }
  }

  /**
   * Grok single Mode control (Shift+Tab ring).
   * - planArmed is orthogonal to permission and persisted per task
   * - permission is the host ACP gate only (spawn/attach may still pass
   *   top-level `grok --permission-mode auto` / `agent --always-approve`)
   *
   * Do NOT send `/auto` or `/always-approve` as session/prompt on chip change.
   * In Grok Build those are local TUI toggles (no turn). Forwarding them over
   * ACP starts a real prompt and can kick off tool runs — different from Grok.
   */
  async function handleSessionModeChange(mode: SessionMode) {
    const sessionId = selectedId;
    const previousPlan = planMode.isArmed(sessionId);
    const previousPerm = effectivePermissionMode;
    const next = applySessionModeChange(mode, previousPerm);
    const planChanged = next.planArmed !== previousPlan;

    if (sessionId && planChanged) {
      planMode.setArmedLocal(sessionId, next.planArmed);
    }

    const targetPerm = next.permission;
    const permChanged = Boolean(targetPerm && targetPerm !== previousPerm);

    if (sessionId && permChanged && targetPerm) {
      setTaskPermissionModes((prev) => ({
        ...prev,
        [sessionId]: targetPerm,
      }));
    }

    if (!planChanged && !permChanged) {
      return;
    }

    const liveHandle =
      managedForSession &&
      managedForSession.status !== "stopped" &&
      managedForSession.status !== "error"
        ? managedForSession.handleId
        : null;

    setError(null);
    try {
      // Sync ACP session mode (wire: plan | default). Permission is separate.
      if (sessionId && liveHandle) {
        await setSessionMode(liveHandle, sessionModeWireId(mode));
      }

      // Persist plan arming separately.
      if (sessionId && planChanged) {
        await setTaskPlanArmed(sessionId, next.planArmed);
      }

      if (permChanged && targetPerm) {
        if (liveHandle) {
          const info = await setPermissionMode(liveHandle, targetPerm);
          upsertManaged(info);
          if (info.sessionId) {
            setTaskPermissionModes((prev) => ({
              ...prev,
              [info.sessionId!]: targetPerm,
            }));
          }
        } else if (sessionId) {
          await setTaskPermissionMode(sessionId, targetPerm);
        }
      }
    } catch (e) {
      if (sessionId) {
        planMode.setArmedLocal(sessionId, previousPlan);
        setTaskPermissionModes((prev) => ({
          ...prev,
          [sessionId]: previousPerm,
        }));
      }
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  async function handleSend(text: string): Promise<SendResult> {
    const trimmed = text.trim();
    if (!trimmed) return { accepted: false };
    setControlBusy(true);
    setError(null);
    setTab("timeline");
    try {
      // Pager builtins (/usage, /context, …) are TUI-local in Grok Build —
      // ACP session/prompt does not render them. Handle here and show in Timeline.
      if (isLocalSlashCommand(trimmed)) {
        const result = await runLocalSlash(trimmed, {
          detail,
          weekUsage,
        });
        if (result) {
          const targetId = selectedId ?? sessions[0]?.id ?? null;
          if (!targetId) {
            // No task to hang Timeline cards on — surface text as a banner.
            const body = result.items
              .map((i) =>
                [i.title, i.detail].filter(Boolean).join("\n"),
              )
              .join("\n\n");
            setError(body || "Command completed.");
          } else {
            if (!selectedId) setSelectedId(targetId);
            appendLocalLive(result.items, targetId);
            setPinTimelineBottomSeq((n) => n + 1);
          }
          if (result.refreshWeekUsage) {
            void refreshWeekUsage({ force: true });
          }
          return { accepted: true };
        }
      }

      // Connect on first agent message (no attach switch). Local slashes above
      // already returned without needing ACP.
      let liveAgent = managedForSession;
      let handleId = liveAgent?.handleId;
      let sessionIdForPlan = liveAgent?.sessionId ?? selectedId;
      if (!handleId || !isLiveManagedStatus(liveAgent?.status)) {
        const sessionId = selectedId ?? sessions[0]?.id ?? null;
        if (!sessionId) {
          setError("Select a task first, or create one with New.");
          return { accepted: false };
        }
        const info = await ensureAttached(sessionId);
        if (!info) {
          // Banner stays off; composer keeps the draft and shows the card cue.
          return { accepted: false, hint: SEND_REFUSAL_HINT.openElsewhere };
        }
        liveAgent = info;
        handleId = info.handleId;
        sessionIdForPlan = info.sessionId ?? sessionId;
      }

      // Reconnect / first attach still Starting. Do not prompt (agent not
      // ready) and do not attach again with ignore_pid = None.
      if (!isLiveManagedStatus(liveAgent?.status)) {
        return { accepted: false, hint: SEND_REFUSAL_HINT.connecting };
      }

      if (liveAgent) {
        try {
          liveAgent = await applySessionModel(liveAgent);
        } catch (e) {
          setError(formatInvokeError(e));
        }
      }

      const { modeError } = await planMode.ensurePlanModeForTurn(
        handleId,
        sessionIdForPlan,
        trimmed,
      );
      if (modeError) {
        // Still send the prompt; surface the mode error so it is not silent.
        setError(modeError);
      }

      // Optimistic Running paint so the left-rail task card updates immediately
      // (agent-status can lose a race with spawn/list Ready snapshots).
      if (liveAgent?.status === "ready") {
        upsertManaged({ ...liveAgent, status: "running" });
      }

      const accepted = await promptAgent(handleId, trimmed);
      if (liveAgent) {
        upsertManaged({
          ...liveAgent,
          status: accepted.status ?? "running",
        });
      }
      setPinTimelineBottomSeq((n) => n + 1);
      return { accepted: true };
    } catch (e) {
      if (isExclusiveSessionError(e)) {
        return { accepted: false, hint: SEND_REFUSAL_HINT.openElsewhere };
      }
      setError(formatInvokeError(e));
      return { accepted: false };
    } finally {
      setControlBusy(false);
    }
  }

  async function applySessionModel(
    info: ManagedAgentInfo,
  ): Promise<ManagedAgentInfo> {
    const applied = await sessionModel.reapply(info);
    if (applied !== info) upsertManaged(applied);
    return applied;
  }

  async function handleModelChange(
    modelId: string,
    reasoningEffort?: string,
  ) {
    const sessionId = managedForSession?.sessionId ?? selectedId;
    if (!sessionId) return;
    const previous = sessionModel.choiceOf(sessionId);
    sessionModel.select(sessionId, modelId, reasoningEffort);
    const live =
      managedForSession && isLiveManagedStatus(managedForSession.status)
        ? managedForSession
        : null;
    if (!live || controlBusy) return;
    setControlBusy(true);
    setError(null);
    try {
      await applySessionModel(live);
    } catch (e) {
      sessionModel.revert(sessionId, previous);
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setControlBusy(false);
    }
  }



  function requestStop(sessionId: string) {
    const managed = managedList.find(
      (m) =>
        m.sessionId === sessionId &&
        m.status !== "stopped",
    );
    if (!managed) return;
    const title =
      sessions.find((s) => s.id === sessionId)?.title ??
      managed.title ??
      "this agent";
    setStopConfirm({
      handleId: managed.handleId,
      sessionId,
      title,
    });
  }

  const confirmStop = useCallback(async () => {
    if (!stopConfirm || controlBusy) return;
    setControlBusy(true);
    setError(null);
    try {
      await stopAgent(stopConfirm.handleId);
      removeManaged(stopConfirm.handleId);
      setStopConfirm(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setControlBusy(false);
    }
  }, [stopConfirm, controlBusy, removeManaged]);

  // Stop dialog: Enter confirms, Escape cancels.
  useEffect(() => {
    if (!stopConfirm) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing) return;
      if (e.key === "Enter") {
        e.preventDefault();
        void confirmStop();
      } else if (e.key === "Escape") {
        e.preventDefault();
        if (!controlBusy) setStopConfirm(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [stopConfirm, confirmStop, controlBusy]);

  // Right workspace rail: Ctrl+H toggles collapse (not Cmd+H — macOS hide app).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.isComposing) return;
      if (
        e.ctrlKey &&
        !e.metaKey &&
        !e.altKey &&
        !e.shiftKey &&
        (e.key === "h" || e.key === "H")
      ) {
        e.preventDefault();
        setWorkspaceCollapsed((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const toggleWorkspaceCollapsed = useCallback(() => {
    setWorkspaceCollapsed((v) => !v);
  }, []);

  const workspaceCollapseBtn = (
    <button
      type="button"
      className="btn ghost workspace-collapse-btn"
      onClick={toggleWorkspaceCollapsed}
      title="Collapse workspace (ctrl+h)"
      aria-label="Collapse workspace panel"
      aria-expanded={!workspaceCollapsed}
      aria-keyshortcuts="Control+H"
    >
      <span className="workspace-collapse-label">Collapse</span>
      <kbd className="shortcut-hint">ctrl+h</kbd>
    </button>
  );

  const defaultCwd = detail?.card.cwd ?? sessions[0]?.cwd ?? "";

  /**
   * sessionId → managed status for left-rail sort + run chrome.
   * Includes `starting`; SessionList ranks it below mid-turn so connect
   * does not jump the card to the top until work actually begins.
   */
  const managedStatuses = useMemo(() => {
    const out: Record<string, (typeof managedList)[number]["status"]> = {};
    for (const m of managedList) {
      if (m.sessionId && m.status !== "stopped" && m.status !== "error") {
        out[m.sessionId] = m.status;
      }
    }
    return out;
  }, [managedList]);

  /** sessionId → pid for left-rail status ribbon (prefer managed process). */
  const managedPids = useMemo(() => {
    const out: Record<string, number> = {};
    for (const m of managedList) {
      if (
        m.sessionId &&
        m.pid != null &&
        m.status !== "stopped" &&
        m.status !== "error"
      ) {
        out[m.sessionId] = m.pid;
      }
    }
    return out;
  }, [managedList]);
  const shownModel = displayedSessionModel(
    sessionModel.choiceOf(selectedId),
    managedForSession,
    detail?.card.modelId ??
      sessions.find((session) => session.id === selectedId)?.modelId,
  );

  return (
    <div className="app-shell">
      <WindowsTitlebar
        onCheckUpdate={checkForUpdate}
        checkStatus={updateCheckStatus}
        onWindowError={setError}
      />
      {(error || lastError) && (
        <div className="banner error-banner">
          <span>{error || lastError}</span>
          <button
            className="btn ghost"
            type="button"
            onClick={() => {
              setError(null);
              clearError();
            }}
          >
            Dismiss
          </button>
        </div>
      )}

      <div
        className={
          "main-grid" + (workspaceCollapsed ? " workspace-collapsed" : "")
        }
      >
        <aside className="left-rail">
          <MacosTitlebarBrand
            onCheckUpdate={checkForUpdate}
            checkStatus={updateCheckStatus}
          />
          <StatsBar
            tokenSeries={tokenSeries}
            weekUsage={weekUsage}
            onRefreshWeekUsage={() => void refreshWeekUsage({ force: true })}
          />
          <SessionList
            sessions={sessions}
            selectedId={selectedId}
            query={query}
            onQuery={setQuery}
            onSelect={(id) => {
              setSelectedId(id);
            }}
            managedStatuses={managedStatuses}
            managedPids={managedPids}
            needsInputSessionIds={needsInputSessionIds}
            onNewTask={() => setModalOpen(true)}
            hasMore={hasMoreSessions}
            onLoadMore={loadMoreSessions}
          />
        </aside>

        <SessionDetailView
          detail={detail}
          loading={detailLoading}
          error={detailError}
          tab={tab}
          onTab={setTab}
          timelineItems={timelineItems}
          timelineHasMore={timelineHistory.hasMore}
          timelineHistoryLoading={timelineHistory.loadingOlder}
          onLoadOlderTimeline={timelineHistory.loadOlder}
          managed={managedForSession}
          permissions={permissionsForSession}
          permBusyKey={permBusyKey}
          controlBusy={controlBusy}
          sessionMode={effectiveSessionMode}
          onSessionModeChange={(m) => void handleSessionModeChange(m)}
          onSendPrompt={handleSend}
          promptQueue={promptQueue}
          onResolvePermission={(item, opt, comments, payload) =>
            void handleResolvePermission(item, opt, comments, payload)
          }
          onStopAgent={
            selectedId && managedForSession && managedForSession.status !== "stopped"
              ? () => requestStop(selectedId)
              : undefined
          }
          pinTimelineBottomSeq={pinTimelineBottomSeq}
          availableCommands={promptCommands}
          onOpenFile={openPreview}
          onCancelSubagent={
            managedForSession &&
            managedForSession.status !== "stopped" &&
            managedForSession.status !== "error"
              ? (subagentId: string) => {
                  void cancelSubagent(
                    managedForSession.handleId,
                    subagentId,
                  ).catch((e) => {
                    setError(e instanceof Error ? e.message : String(e));
                  });
                }
              : undefined
          }
          onKillTask={
            managedForSession &&
            managedForSession.status !== "stopped" &&
            managedForSession.status !== "error"
              ? (taskId: string) => {
                  void killTask(managedForSession.handleId, taskId).catch(
                    (e) => {
                      setError(e instanceof Error ? e.message : String(e));
                    },
                  );
                }
              : undefined
          }
          onModelChange={handleModelChange}
          modelId={shownModel.modelId}
          reasoningEffort={shownModel.reasoningEffort}
        />

        <aside
          className={
            "side-panel workspace-panel" +
            (workspaceCollapsed ? " is-collapsed" : "")
          }
          aria-label="Workspace"
        >
          {workspaceCollapsed ? (
            <button
              type="button"
              className="workspace-expand-rail"
              onClick={toggleWorkspaceCollapsed}
              title="Show workspace (ctrl+h)"
              aria-label="Show workspace panel"
              aria-expanded={false}
              aria-keyshortcuts="Control+H"
            >
              <span className="workspace-expand-chevron" aria-hidden>
                ‹
              </span>
              <span className="workspace-expand-text">Workspace</span>
            </button>
          ) : (
            <WorkspacePanel
              cwd={projectCwd}
              refreshKey={gitRefreshKey}
              previewPath={previewPath}
              onPreviewPath={openPreview}
              sessionId={selectedId}
              collapseControl={workspaceCollapseBtn}
            />
          )}
        </aside>
      </div>

      <NewTaskModal
        open={modalOpen}
        defaultCwd={defaultCwd}
        busy={controlBusy}
        defaultSessionMode={lastSpawnSessionMode}
        onClose={() => setModalOpen(false)}
        onSubmit={(o) => void handleSpawn(o)}
      />

      <UpdateModal update={pendingUpdate} onDismiss={dismissUpdate} />

      {stopConfirm && (
        <div
          className="modal-backdrop"
          onClick={() => {
            if (!controlBusy) setStopConfirm(null);
          }}
        >
          <div
            className="modal stop-confirm-modal"
            onClick={(e) => e.stopPropagation()}
            role="dialog"
            aria-modal
            aria-labelledby="stop-confirm-title"
          >
            <div className="modal-header">
              <h2 id="stop-confirm-title">Stop agent?</h2>
            </div>
            <p className="muted small">
              This will kill the agent process for{" "}
              <strong title={stopConfirm.title}>{stopConfirm.title}</strong>
              {" "}and cancel any pending permission requests. Session history
              on disk is kept. Send a message later to reconnect.
            </p>
            <div className="modal-actions">
              <button
                className="btn"
                type="button"
                disabled={controlBusy}
                onClick={() => setStopConfirm(null)}
              >
                Cancel
              </button>
              <button
                className="btn danger-btn"
                type="button"
                disabled={controlBusy}
                autoFocus
                onClick={() => void confirmStop()}
              >
                {controlBusy ? "Stopping…" : "Stop agent"}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default App;
