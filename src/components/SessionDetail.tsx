import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import type {
  AvailableCommand,
  TimelineItem,
  MainTab,
  ManagedAgentInfo,
  PendingPermission,
  SessionMode,
  SessionDetail as Detail,
} from "../types";
import {
  contextPct,
  formatDuration,
  formatRelative,
  formatTokens,
  shortPath,
} from "../utils/format";
import {
  listActiveBgTasks,
  listActiveSubagents,
} from "../utils/subagentTasks";
import type { ResolvePermissionFn } from "../utils/permissionPayload";
import type { PromptQueueController } from "../hooks/usePromptQueueController";
import { DiffPanel } from "./DiffPanel";
import { PermissionGate } from "./PermissionGate";
import { PromptBar, type SendResult } from "./PromptBar";
import { PromptQueue } from "./PromptQueue";
import { TimelinePanel } from "./TimelinePanel";
import { TurnStatusBar } from "./TurnStatusBar";

interface Props {
  detail: Detail | null;
  loading: boolean;
  error: string | null;
  tab: MainTab;
  onTab: (t: MainTab) => void;
  timelineItems: TimelineItem[];
  timelineHasMore: boolean;
  timelineHistoryLoading: boolean;
  onLoadOlderTimeline: () => Promise<void>;
  managed: ManagedAgentInfo | null;
  permissions: PendingPermission[];
  permBusyKey: string | null;
  controlBusy: boolean;
  sessionMode: SessionMode;
  onSessionModeChange: (mode: SessionMode) => void;
  onSendPrompt: (text: string) => void | Promise<void | SendResult>;
  promptQueue: PromptQueueController;
  onResolvePermission: ResolvePermissionFn;
  /** Stop the live agent for this task (confirm handled by parent). */
  onStopAgent?: () => void;
  /** Bump after connect/spawn to pin Timeline to the bottom. */
  pinTimelineBottomSeq?: number;
  /** Agent-advertised slash commands for the prompt autocomplete. */
  availableCommands?: AvailableCommand[];
  /** Open a project file path in the right-rail preview pane. */
  onOpenFile?: (path: string) => void;
  /** Cancel a running subagent by its id. */
  onCancelSubagent?: (subagentId: string) => void;
  /** Kill a running background task by its id. */
  onKillTask?: (taskId: string) => void;
  /** Switch session model or reasoning level via ACP set_session_model. */
  onModelChange?: (modelId: string, reasoningEffort?: string) => void;
  /** Resolved model id for the chip (user pick, else agent, else card). */
  modelId?: string | null;
  /** Resolved thinking level for the chip. */
  reasoningEffort?: string | null;
}

export function SessionDetailView({
  detail,
  loading,
  error,
  tab,
  onTab,
  timelineItems,
  timelineHasMore,
  timelineHistoryLoading,
  onLoadOlderTimeline,
  managed,
  permissions,
  permBusyKey,
  controlBusy,
  sessionMode,
  onSessionModeChange,
  onSendPrompt,
  promptQueue,
  onResolvePermission,
  onStopAgent,
  pinTimelineBottomSeq = 0,
  availableCommands = [],
  onOpenFile,
  onCancelSubagent,
  onKillTask,
  onModelChange,
  modelId = null,
  reasoningEffort = null,
}: Props) {
  const tabBodyRef = useRef<HTMLDivElement>(null);
  const [sendRefusalHint, setSendRefusalHint] = useState<string | null>(null);

  useEffect(() => {
    setSendRefusalHint(null);
  }, [detail?.card.id]);

  // Timeline pins to bottom; Diff / Raw expect top. Shared .tab-body
  // scroll container otherwise keeps Timeline scrollTop and hides content.
  useLayoutEffect(() => {
    if (tab === "timeline") return;
    const el = tabBodyRef.current;
    if (!el) return;
    el.scrollTop = 0;
  }, [tab, detail?.card.id]);

  if (loading && !detail) {
    return (
      <section className="detail-panel">
        <div className="empty-state">Loading session…</div>
      </section>
    );
  }

  if (error && !detail) {
    return (
      <section className="detail-panel">
        <div className="empty-state error-text">{error}</div>
      </section>
    );
  }

  if (!detail) {
    return (
      <section className="detail-panel">
        <div className="empty-state">
          <h2>Select a task</h2>
          <p>
            Pick a session from the left, or create one with{" "}
            <strong>New</strong>. Timeline mirrors Grok Build on disk; send a
            message to connect live.
          </p>
        </div>
      </section>
    );
  }

  const { card } = detail;
  const pct = contextPct(card.contextTokensUsed, card.contextWindowTokens);
  const canStop =
    Boolean(onStopAgent) &&
    managed &&
    managed.status !== "stopped" &&
    managed.status !== "stopping";

  return (
    <section className="detail-panel">
      <PermissionGate
        items={permissions}
        busyKey={permBusyKey}
        onResolve={onResolvePermission}
      />

      <div className="detail-header">
        <div className="detail-header-main">
          {/* The blank title-row space is a macOS Overlay drag affordance. */}
          <div className="detail-title-row">
            <h1 className="text-title-gradient" title={card.title || undefined}>{card.title}</h1>
            <div className="detail-title-drag-region" data-tauri-drag-region aria-hidden />
          </div>
          <div className="detail-sub">
            <span title={card.cwd}>{shortPath(card.cwd, 64)}</span>
            {card.headBranch && (
              <span title={card.headBranch}>⎇ {card.headBranch}</span>
            )}
            <span>
              updated {formatRelative(card.lastActiveAt ?? card.updatedAt)}
            </span>
          </div>
          <SubagentTaskStrip
            items={timelineItems}
            onCancelSubagent={onCancelSubagent}
            onKillTask={onKillTask}
          />
        </div>

        <div className="metric-grid">
          <Metric
            label="Context"
            value={`${formatTokens(card.contextTokensUsed, { decimals: false })} / ${formatTokens(card.contextWindowTokens, { decimals: false })}`}
            bar={Math.round(pct)}
          />
          <Metric label="Turns" value={String(card.turnCount)} />
          <Metric
            label="Diff"
            value={`+${card.agentLinesAdded} / −${card.agentLinesRemoved}`}
          />
          <Metric
            label="Duration"
            value={formatDuration(card.sessionDurationSeconds)}
          />
        </div>
      </div>

      <div className="tabs">
        {(
          [
            ["timeline", "Timeline"],
            ["diff", "File changes"],
            ["raw", "Raw stream"],
          ] as const
        ).map(([id, label]) => (
          <button
            key={id}
            className={tab === id ? "active" : ""}
            onClick={() => onTab(id)}
          >
            {label}
            {id === "timeline" && timelineItems.length > 0 && (
              <span className="tab-count">{timelineItems.length}</span>
            )}
            {id === "diff" && detail.recentHunks.hunks.length > 0 && (
              <span className="tab-count">
                {detail.recentHunks.hunks.length}
                {detail.recentHunks.hasMore ? "+" : ""}
              </span>
            )}
          </button>
        ))}
      </div>

      <div
        className={`tab-body${tab === "timeline" ? " tab-body-timeline" : ""}`}
        ref={tabBodyRef}
      >
        {tab === "timeline" && (
          <TimelinePanel
            items={timelineItems}
            managed={managed}
            pinBottomSeq={pinTimelineBottomSeq}
            onOpenFile={onOpenFile}
            hasMore={timelineHasMore}
            loadingOlder={timelineHistoryLoading}
            onLoadOlder={onLoadOlderTimeline}
          />
        )}
        {tab === "diff" && (
          <DiffPanel
            hunks={detail.recentHunks.hunks}
            totalFilesTouched={card.agentFilesTouched}
            hasMore={detail.recentHunks.hasMore}
            onOpenFile={onOpenFile}
          />
        )}
        {tab === "raw" && <RawStream detail={detail} />}
      </div>

      <PromptQueue controller={promptQueue} />
      <TurnStatusBar
        managed={managed}
        timelineItems={timelineItems}
        sessionIsActive={Boolean(card?.isActive)}
        refusalHint={sendRefusalHint}
      />
      <PromptBar
        managed={managed}
        busy={controlBusy}
        sessionMode={sessionMode}
        onSessionModeChange={onSessionModeChange}
        onSend={onSendPrompt}
        onRefusal={setSendRefusalHint}
        availableCommands={availableCommands}
        timelineItems={timelineItems}
        sessionId={card?.id ?? null}
        modelId={modelId}
        availableModels={managed?.availableModels ?? []}
        reasoningEffort={reasoningEffort}
        onModelChange={onModelChange}
        canStop={Boolean(canStop)}
        onStop={onStopAgent}
      />

    </section>
  );
}

function Metric({
  label,
  value,
  bar,
}: {
  label: string;
  value: string;
  /** 0–100: fill the whole block as a progress background (no inner bar). */
  bar?: number;
}) {
  const fill =
    typeof bar === "number"
      ? Math.max(0, Math.min(100, Math.round(bar)))
      : null;
  return (
    <div
      className={`metric${fill != null ? " metric-progress" : ""}`}
      style={
        fill != null
          ? ({ "--metric-pct": `${fill}%` } as CSSProperties)
          : undefined
      }
      title={fill != null ? `${fill}% context used` : undefined}
    >
      <div className="metric-label">{label}</div>
      <div className="metric-value">{value}</div>
    </div>
  );
}

/**
 * Live chips for running child subagents + background tasks.
 * Mirrors Grok Build's tasks-pane summary without a second full pane.
 */
function SubagentTaskStrip({
  items,
  onCancelSubagent,
  onKillTask,
}: {
  items: TimelineItem[];
  onCancelSubagent?: (subagentId: string) => void;
  onKillTask?: (taskId: string) => void;
}) {
  const subs = useMemo(() => listActiveSubagents(items), [items]);
  const tasks = useMemo(() => listActiveBgTasks(items), [items]);
  if (subs.length === 0 && tasks.length === 0) return null;

  return (
    <div className="subagent-task-strip" aria-label="Active subagents and tasks">
      {subs.map((s) => (
        <span
          key={s.childSessionId}
          className="subagent-task-chip subagent-chip"
          title={[
            s.description,
            s.subagentType,
            s.model,
            s.childSessionId,
            onCancelSubagent ? "Click to cancel" : "",
          ]
            .filter(Boolean)
            .join(" · ")}
          onClick={
            onCancelSubagent
              ? () => onCancelSubagent(s.subagentId)
              : undefined
          }
          role={onCancelSubagent ? "button" : undefined}
          tabIndex={onCancelSubagent ? 0 : undefined}
          onKeyDown={
            onCancelSubagent
              ? (e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onCancelSubagent(s.subagentId);
                  }
                }
              : undefined
          }
        >
          <span className="subagent-task-chip-mark" aria-hidden>
            ◆
          </span>
          {s.description || s.subagentType}
          <span className="subagent-task-chip-meta">
            {s.subagentType}
            {s.activityLabel ? ` · ${s.activityLabel}` : ""}
          </span>
          {onCancelSubagent && (
            <span className="subagent-task-chip-close" aria-hidden title="Cancel subagent">
              ✕
            </span>
          )}
        </span>
      ))}
      {tasks.map((t) => (
        <span
          key={t.taskId}
          className={`subagent-task-chip task-chip${
            t.isMonitor ? " is-monitor" : ""
          }`}
          title={[
            t.command,
            t.taskId,
            t.cwd,
            onKillTask ? "Click to kill" : "",
          ]
            .filter(Boolean)
            .join(" · ")}
          onClick={
            onKillTask ? () => onKillTask(t.taskId) : undefined
          }
          role={onKillTask ? "button" : undefined}
          tabIndex={onKillTask ? 0 : undefined}
          onKeyDown={
            onKillTask
              ? (e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    onKillTask(t.taskId);
                  }
                }
              : undefined
          }
        >
          <span className="subagent-task-chip-mark" aria-hidden>
            ▣
          </span>
          {t.isMonitor ? "Monitor" : "Task"}:{" "}
          {t.description?.trim() || t.command || t.taskId}
          {onKillTask && (
            <span className="subagent-task-chip-close" aria-hidden title="Kill task">
              ✕
            </span>
          )}
        </span>
      ))}
    </div>
  );
}

function RawStream({ detail }: { detail: Detail }) {
  const sample = detail.recentUpdates.slice(-5);
  return (
    <div className="raw-stream">
      <p className="muted small">
        Last {sample.length} ACP <code>session/update</code> records (tail of
        updates.jsonl).
      </p>
      <pre>{JSON.stringify(sample, null, 2)}</pre>
    </div>
  );
}
