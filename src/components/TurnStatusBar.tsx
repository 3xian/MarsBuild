import { useEffect, useMemo, useRef, useState } from "react";
import type { ManagedAgentInfo, TimelineItem } from "../types";
import { getSessionUsage } from "../api";
import { formatTokens } from "../utils/format";
import {
  formatPhaseTimer,
  resolveTurnActivity,
  resolveTurnStartedAt,
  sendRefusalActivity,
  type TurnIndicatorMode,
} from "../utils/turnActivity";

interface Props {
  managed: ManagedAgentInfo | null;
  timelineItems: TimelineItem[];
  /**
   * Session process is open (Grok `active_sessions` / card.isActive).
   * Shows an ambient status when PinkCode is not mid-turn on this session.
   */
  sessionIsActive?: boolean;
  /** Replace the strip with a Disconnected-style refusal after a dropped send. */
  refusalHint?: string | null;
}

/** Phase / turn timer tick. */
const CLOCK_MS = 100;
/** Live usage poll interval (only when turn is running). */
const USAGE_POLL_MS = 10_000;

/**
 * Live turn status above the prompt.
 * Managed mid-turn → rich activity; external open session → ambient cue.
 */
export function TurnStatusBar({
  managed,
  timelineItems,
  sessionIsActive = false,
  refusalHint = null,
}: Props) {
  const activity = useMemo(() => {
    if (refusalHint != null) return sendRefusalActivity(refusalHint);
    return resolveTurnActivity(managed, timelineItems, {
      sessionIsActive,
    });
  }, [managed, timelineItems, sessionIsActive, refusalHint]);

  const [now, setNow] = useState(() => Date.now());
  const [phaseStartedAt, setPhaseStartedAt] = useState(() => Date.now());
  const [turnAnchor, setTurnAnchor] = useState<number | null>(null);
  const lastPhaseKey = useRef<string | null>(null);

  // Live turn usage polling
  const [liveTokens, setLiveTokens] = useState<number | null>(null);
  const [liveCostTicks, setLiveCostTicks] = useState<number | null>(null);

  useEffect(() => {
    if (!activity || !managed) {
      setLiveTokens(null);
      setLiveCostTicks(null);
      return;
    }
    let alive = true;
    const poll = async () => {
      try {
        const u = await getSessionUsage(managed.handleId);
        if (!alive) return;
        setLiveTokens(u.totalTokens);
        setLiveCostTicks(u.costUsdTicks);
      } catch {
        // silently ignore — usage poll is best-effort
      }
    };
    void poll();
    const id = window.setInterval(poll, USAGE_POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(id);
    };
  }, [activity, managed]);

  // Reset phase / turn anchors when activity phase changes (Grok-style).
  useEffect(() => {
    if (!activity) {
      lastPhaseKey.current = null;
      setTurnAnchor(null);
      return;
    }
    if (lastPhaseKey.current !== activity.phaseKey) {
      lastPhaseKey.current = activity.phaseKey;
      setPhaseStartedAt(Date.now());
    }
    if (turnAnchor == null && activity.showTurnTimer) {
      setTurnAnchor(resolveTurnStartedAt(timelineItems, Date.now()));
    }
  }, [activity, timelineItems, turnAnchor]);

  useEffect(() => {
    if (!activity) return;
    if (!activity.showPhaseTimer && !activity.showTurnTimer) return;
    const clockId = window.setInterval(() => setNow(Date.now()), CLOCK_MS);
    return () => window.clearInterval(clockId);
  }, [activity?.phaseKey, activity?.showPhaseTimer, activity?.showTurnTimer]);

  if (!activity) return null;

  const phaseMs = Math.max(0, now - phaseStartedAt);
  const turnStart = turnAnchor ?? phaseStartedAt;
  const turnMs = Math.max(0, now - turnStart);
  const phaseLabel = formatPhaseTimer(phaseMs);
  const showPhase =
    activity.showPhaseTimer && activity.indicator !== "wait";
  // Hide turn timer when it would duplicate a short phase (same second window).
  const showTurn =
    activity.showTurnTimer && turnMs >= 1000 && turnMs - phaseMs >= 500;
  const mode = activity.indicator;

  const costLabel =
    liveCostTicks != null
      ? `$${(liveCostTicks / 1e10).toFixed(4)}`
      : null;

  return (
    <div
      className={`turn-status tone-${activity.tone} mode-${mode} source-${activity.source}`}
      role="status"
      aria-live="polite"
      aria-atomic="true"
    >
      <div className="turn-status-left">
        <ActivityIndicator mode={mode} />
        <div className="turn-status-copy">
          <span className="turn-status-label">
            <span className="turn-status-label-main">{activity.label}</span>
            {activity.detail ? (
              <span className="turn-status-label-detail" title={activity.detail}>
                {activity.detail}
              </span>
            ) : null}
          </span>
          {activity.hint ? (
            <span className="turn-status-hint">{activity.hint}</span>
          ) : null}
        </div>
        {showPhase && (
          <span className="turn-status-phase" aria-hidden>
            {phaseLabel}
          </span>
        )}
      </div>
      <div className="turn-status-right">
        {liveTokens != null && (
          <span className="turn-status-tokens" title="Live turn tokens">
            {formatTokens(liveTokens)}
          </span>
        )}
        {costLabel && (
          <span className="turn-status-cost" title="Estimated turn cost">
            {costLabel}
          </span>
        )}
        {showTurn && (
          <span className="turn-status-turn" title="Turn elapsed">
            {formatPhaseTimer(turnMs)}
          </span>
        )}
      </div>
    </div>
  );
}

/** Busy spin, wait pulse, cancel spin, or a static still dot. */
function ActivityIndicator({ mode }: { mode: TurnIndicatorMode }) {
  return (
    <span className={`turn-mark mode-${mode}`} aria-hidden>
      <span className="turn-mark-ring" />
      <span className="turn-mark-dot" />
    </span>
  );
}
