import type { ManagedAgentInfo, TimelineItem } from "../types";
import {
  isManagedTurnActive,
  isPinkcodeAttached,
} from "./managedChrome";

export { isManagedTurnActive } from "./managedChrome";

/**
 * Phase timer format matching Grok Build `format_duration`:
 * - under 10s → `5.2s`
 * - 10–59s → `32s`
 * - 1–59m → `2m5s`
 * - 1h+ → `1h2m`
 */
export function formatPhaseTimer(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "0.0s";
  const totalSecs = ms / 1000;
  if (totalSecs < 10) return `${totalSecs.toFixed(1)}s`;
  if (totalSecs < 60) return `${Math.floor(totalSecs)}s`;
  const mins = Math.floor(totalSecs / 60);
  const secs = Math.floor(totalSecs % 60);
  if (mins < 60) return `${mins}m${secs}s`;
  const hours = Math.floor(mins / 60);
  const remainingMins = mins % 60;
  return `${hours}h${remainingMins}m`;
}

/** Visual tone for the status row (indicator + label color). */
export type TurnActivityTone =
  | "muted"
  | "tool"
  | "wait"
  | "danger"
  | "user";

export type TurnActivityKind =
  | "starting"
  | "thinking"
  | "responding"
  | "tool"
  | "waiting"
  | "waitingUser"
  | "cancelling"
  | "external";

export type TurnActivitySource = "managed" | "external";

/** Indicator motion: busy spin, calm pulse, snappy cancel, or static. */
export type TurnIndicatorMode = "spin" | "wait" | "danger" | "still";

export interface ResolvedTurnActivity {
  kind: TurnActivityKind;
  /** Primary label, e.g. `Thinking…` or `Run `. */
  label: string;
  /** Optional command/detail after a muted prefix (`Run ` + detail). */
  detail?: string;
  tone: TurnActivityTone;
  indicator: TurnIndicatorMode;
  /**
   * Stable key for phase-timer resets — changes when the activity phase changes.
   */
  phaseKey: string;
  /** Where the activity signal came from. */
  source: TurnActivitySource;
  /** Optional secondary hint (e.g. how to connect). */
  hint?: string;
  /** Show the phase elapsed chip next to the label. */
  showPhaseTimer: boolean;
  /** Show the whole-turn elapsed on the right. */
  showTurnTimer: boolean;
}

function isManagedConnected(managed?: ManagedAgentInfo | null): boolean {
  return Boolean(managed && isPinkcodeAttached(managed.status));
}

function isTerminalToolStatus(status?: string | null): boolean {
  if (!status) return false;
  const s = status.toLowerCase();
  return (
    s === "completed" ||
    s === "failed" ||
    s === "cancelled" ||
    s === "canceled" ||
    s === "error"
  );
}

/**
 * Whether a tool/shell row looks in-flight.
 * Managed mid-turn: missing status is optimistic-active (stream may lag).
 * External/disk hydrate: missing status is not active (history is usually settled).
 */
function isActiveToolStatus(
  status: string | null | undefined,
  source: TurnActivitySource,
): boolean {
  if (!status) return source === "managed";
  return !isTerminalToolStatus(status);
}

function stripToolStatusSuffix(title: string): string {
  // Titles may end with " · completed" / " ✓" from composeToolTitle.
  return title
    .replace(
      /\s*[·•]\s*(completed|failed|cancelled|canceled|pending|in_progress)\s*$/i,
      "",
    )
    .replace(/\s*[✓✗×]\s*$/u, "")
    .trim();
}

function isTurnTerminalEvent(item: TimelineItem): boolean {
  if (item.kind !== "event") return false;
  const t = (item.title || "").toLowerCase();
  return (
    t.startsWith("worked for") ||
    t.startsWith("turn completed") ||
    t.startsWith("turn failed") ||
    t.startsWith("turn cancelled") ||
    t.startsWith("rate limited")
  );
}

function activity(
  partial: Omit<
    ResolvedTurnActivity,
    "showPhaseTimer" | "showTurnTimer" | "indicator"
  > &
    Partial<
      Pick<
        ResolvedTurnActivity,
        "showPhaseTimer" | "showTurnTimer" | "indicator" | "detail" | "hint"
      >
    >,
): ResolvedTurnActivity {
  const tone = partial.tone;
  const indicator =
    partial.indicator ??
    (tone === "danger" ? "danger" : tone === "user" || tone === "wait" ? "wait" : "spin");
  return {
    kind: partial.kind,
    label: partial.label,
    detail: partial.detail,
    tone,
    indicator,
    phaseKey: partial.phaseKey,
    source: partial.source,
    hint: partial.hint,
    showPhaseTimer: partial.showPhaseTimer ?? true,
    showTurnTimer: partial.showTurnTimer ?? true,
  };
}

/** Muted reason under red `Not sent`. Same shape as Disconnected + lastError. */
export const SEND_REFUSAL_HINT = {
  openElsewhere: "Already open in Grok Build",
  connecting: "Still connecting",
} as const;

/** Composer refused a send. Same chrome as ACP Disconnected (red, on the box). */
export function sendRefusalActivity(hint?: string | null): ResolvedTurnActivity {
  const trimmed = hint?.trim();
  return activity({
    kind: "waiting",
    label: "Not sent",
    tone: "danger",
    indicator: "still",
    phaseKey: "send-refusal",
    source: "managed",
    hint: trimmed || undefined,
    showPhaseTimer: false,
    showTurnTimer: false,
  });
}

function waiting(
  source: TurnActivitySource,
  phaseKey: string,
): ResolvedTurnActivity {
  return activity({
    kind: "waiting",
    label: "Waiting for response…",
    tone: "wait",
    phaseKey,
    source,
  });
}

/**
 * Infer activity from the timeline tail (ACP live stream or disk hydrate).
 * Returns null when the tail does not indicate in-flight work.
 */
export function inferActivityFromTimeline(
  items: TimelineItem[],
  source: TurnActivitySource,
): ResolvedTurnActivity | null {
  for (let i = items.length - 1; i >= 0; i--) {
    const item = items[i];
    if (!item) continue;

    if (item.kind === "user") {
      // Prompt boundary with nothing after → waiting on model only if managed
      // (external: just means idle open session).
      return null;
    }

    if (isTurnTerminalEvent(item)) return null;
    if (item.kind === "event" || item.kind === "commands") continue;

    if (item.kind === "thought") {
      return activity({
        kind: "thinking",
        label: "Thinking…",
        tone: "muted",
        phaseKey: `thinking:${item.id}`,
        source,
      });
    }

    if (item.kind === "agent") {
      if (item.streaming) {
        return activity({
          kind: "responding",
          label: "Responding…",
          tone: "muted",
          phaseKey: `responding:${item.id}`,
          source,
        });
      }
      // Settled agent text: mid-turn only when managed is driving the turn.
      return source === "managed"
        ? waiting(source, `waiting-after-agent:${item.id}`)
        : null;
    }

    if (item.kind === "shell" && item.shell) {
      const st = item.shell.status;
      if (isActiveToolStatus(st, source)) {
        const desc = item.shell.description?.trim();
        const cmd = item.shell.command?.trim() || "shell";
        if (desc) {
          return activity({
            kind: "tool",
            label: `${desc}…`,
            tone: "muted",
            phaseKey: `shell:${item.shell.toolCallId || item.id}`,
            source,
          });
        }
        return activity({
          kind: "tool",
          label: "Run ",
          detail: cmd,
          tone: "tool",
          phaseKey: `shell:${item.shell.toolCallId || item.id}`,
          source,
        });
      }
      return source === "managed"
        ? waiting(source, `waiting-after-shell:${item.id}`)
        : null;
    }

    // Grok Build WaitingReason::Subagent — blocked on a child agent.
    if (item.kind === "subagent" && item.subagent) {
      const st = item.subagent.status;
      if (st === "running" || isActiveToolStatus(st, source)) {
        const desc = item.subagent.description?.trim() || "subagent";
        return activity({
          kind: "waiting",
          label: "Waiting on subagent…",
          detail: desc,
          tone: "wait",
          phaseKey: `subagent:${item.subagent.childSessionId || item.id}`,
          source,
        });
      }
      return source === "managed"
        ? waiting(source, `waiting-after-subagent:${item.id}`)
        : null;
    }

    // Background bash/monitor (TaskTool family status surface).
    if (item.kind === "task" && item.task) {
      const st = item.task.status;
      if (st === "running" || isActiveToolStatus(st, source)) {
        const subject =
          item.task.description?.trim() ||
          item.task.command?.trim() ||
          "background task";
        return activity({
          kind: "waiting",
          label: item.task.isMonitor
            ? "Waiting on monitor…"
            : "Waiting on background task…",
          detail: subject,
          tone: "wait",
          phaseKey: `task:${item.task.taskId || item.id}`,
          source,
        });
      }
      return source === "managed"
        ? waiting(source, `waiting-after-task:${item.id}`)
        : null;
    }

    if (item.kind === "tool") {
      const st = item.toolStatus;
      const base =
        (item.toolBase && item.toolBase.trim()) ||
        stripToolStatusSuffix(item.title || "tool");
      if (isActiveToolStatus(st, source)) {
        if (base.startsWith("Ask:") || base.startsWith("Ask ")) {
          const detail =
            base.replace(/^Ask:\s*/i, "").replace(/^Ask\s+/i, "").trim() ||
            "questions";
          return activity({
            kind: "waitingUser",
            label: `Waiting on answers for ${detail}`,
            tone: "user",
            phaseKey: `ask:${item.toolCallId || item.id}`,
            source,
            showPhaseTimer: false,
          });
        }
        return activity({
          kind: "tool",
          label: "Run ",
          detail: base,
          tone: "tool",
          phaseKey: `tool:${item.toolCallId || item.id}`,
          source,
        });
      }
      return source === "managed"
        ? waiting(source, `waiting-after-tool:${item.id}`)
        : null;
    }

    if (item.kind === "plan") {
      return activity({
        kind: "thinking",
        label: "Thinking…",
        tone: "muted",
        phaseKey: `plan:${item.id}`,
        source,
      });
    }
  }

  return null;
}

export interface ResolveTurnActivityOptions {
  /**
   * Session process is open (Grok `active_sessions` / card.isActive).
   * Used when PinkCode is not mid-turn on this session.
   */
  sessionIsActive?: boolean;
}

/**
 * Resolve the turn-status line.
 *
 * Priority:
 * 1. PinkCode managed mid-turn (`running` / permission / start / stop)
 * 2. Session open in Grok Build (or another host) without PinkCode mid-turn
 *    — ambient "Open in Grok Build", or timeline-inferred activity from disk
 */
export function resolveTurnActivity(
  managed: ManagedAgentInfo | null | undefined,
  items: TimelineItem[],
  opts?: ResolveTurnActivityOptions,
): ResolvedTurnActivity | null {
  if (managed && isManagedTurnActive(managed.status)) {
    if (managed.status === "starting") {
      return activity({
        kind: "starting",
        label: "Starting session…",
        tone: "muted",
        phaseKey: "starting",
        source: "managed",
        showTurnTimer: false,
      });
    }

    if (managed.status === "stopping") {
      return activity({
        kind: "cancelling",
        label: "Cancelling…",
        tone: "danger",
        phaseKey: "cancelling",
        source: "managed",
        showTurnTimer: false,
      });
    }

    if (managed.status === "awaitingPermission") {
      return activity({
        kind: "waitingUser",
        label: "Waiting for approval…",
        tone: "user",
        phaseKey: "awaitingPermission",
        source: "managed",
        showPhaseTimer: false,
      });
    }

    // status === "running"
    return (
      inferActivityFromTimeline(items, "managed") ??
      waiting("managed", "waiting:model")
    );
  }

  // PinkCode already attached but idle — no status row.
  if (isManagedConnected(managed)) return null;

  // Our attach died. Do not relabel the leftover grok pid as Grok Build.
  if (managed?.status === "error") {
    return activity({
      kind: "waiting",
      label: "Disconnected",
      tone: "danger",
      indicator: "danger",
      phaseKey: "error",
      source: "managed",
      hint: managed.lastError?.trim()
        ? managed.lastError
        : "Agent lost ACP. Stop, then send to reconnect.",
      showPhaseTimer: false,
      showTurnTimer: false,
    });
  }

  // External host has the session process open (typical: Grok Build TUI).
  if (opts?.sessionIsActive) {
    const inferred = inferActivityFromTimeline(items, "external");
    if (inferred) {
      return {
        ...inferred,
        hint: "Open in Grok Build · send a message here to connect",
      };
    }
    return activity({
      kind: "external",
      label: "Open in Grok Build",
      tone: "muted",
      indicator: "wait",
      phaseKey: "external-open",
      source: "external",
      hint: "Send a message to connect live",
      showPhaseTimer: false,
      showTurnTimer: false,
    });
  }

  return null;
}

/** Wall-clock start of the current turn: last user card timestamp, if recent. */
export function resolveTurnStartedAt(
  items: TimelineItem[],
  fallbackMs: number,
): number {
  for (let i = items.length - 1; i >= 0; i--) {
    const item = items[i];
    if (item?.kind === "user" && item.ts > 0) {
      // Ignore absurdly stale user cards (clock skew / hydrated history).
      if (fallbackMs - item.ts <= 24 * 60 * 60 * 1000) {
        return item.ts;
      }
      break;
    }
  }
  return fallbackMs;
}
