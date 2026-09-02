import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listSessionUpdates } from "../api";
import type { SessionDetail } from "../types";
import { extractUpdateEventId } from "../utils/format";

const TIMELINE_PAGE_SIZE = 250;

type TimelineHistoryState = {
  updates: unknown[];
  cursor: number | null;
  hasMore: boolean;
};

export interface TimelineHistoryController {
  hasMore: boolean;
  loadingOlder: boolean;
  loadOlder: () => Promise<void>;
}

function updateIdentity(update: unknown): string {
  return extractUpdateEventId(update) ?? `json:${JSON.stringify(update)}`;
}

export function mergeUpdatePages(
  older: unknown[],
  newer: unknown[],
): unknown[] {
  const merged = new Map<string, unknown>();
  for (const update of older) merged.set(updateIdentity(update), update);
  for (const update of newer) merged.set(updateIdentity(update), update);
  return Array.from(merged.values());
}

/**
 * Cheap fingerprint of an updates page so detail polls can skip React state
 * churn when the page identity is unchanged.
 *
 * Heuristic: length + first/mid/last event ids. Prefers false negatives
 * (extra state update) over false positives (skipping a real change).
 */
export function updatesPageFingerprint(updates: unknown[]): string {
  if (updates.length === 0) return "0";
  const first = updateIdentity(updates[0]);
  const last = updateIdentity(updates[updates.length - 1]);
  const mid = updateIdentity(updates[updates.length >> 1]);
  return `${updates.length}:${first}:${mid}:${last}`;
}

/**
 * Disk timeline pages for the selected session.
 *
 * Hydrate ownership:
 * - First page available for a session visit → hydrate
 * - "Load earlier activity" → hydrate (always; uses latest map merge)
 * - Silent detail / FS polls:
 *   - when `liveOwnsTail` (ACP attached, including starting) → raw page only
 *   - when disk-only → re-hydrate when the page fingerprint changes
 */
export function useTimelineHistory(
  sessionId: string | null,
  detail: SessionDetail | null,
  hydrateDiskLive: (sessionId: string, updates: unknown[]) => void,
  liveOwnsTail = false,
): TimelineHistoryController {
  const [bySession, setBySession] = useState<
    Map<string, TimelineHistoryState>
  >(() => new Map());
  const [loadingSessionId, setLoadingSessionId] = useState<string | null>(null);
  /** sessionId → fingerprint of the last successful disk hydrate this visit. */
  const lastHydratedFp = useRef(new Map<string, string>());
  const hydrateDiskLiveRef = useRef(hydrateDiskLive);
  hydrateDiskLiveRef.current = hydrateDiskLive;

  useEffect(() => {
    if (!detail || detail.card.id !== sessionId) return;
    const id = detail.card.id;
    setBySession((previous) => {
      const current = previous.get(id);
      const nextState: TimelineHistoryState = current
        ? {
            ...current,
            // Preserve loadOlder cursor/hasMore; only fold in the latest tail page.
            updates: mergeUpdatePages(current.updates, detail.recentUpdates),
          }
        : {
            updates: detail.recentUpdates,
            cursor: detail.recentUpdatesCursor ?? null,
            hasMore: detail.recentUpdatesHasMore,
          };
      if (
        current &&
        current.cursor === nextState.cursor &&
        current.hasMore === nextState.hasMore &&
        updatesPageFingerprint(current.updates) ===
          updatesPageFingerprint(nextState.updates)
      ) {
        return previous;
      }
      const next = new Map<string, TimelineHistoryState>();
      // Keep only the active session's raw page (memory bound).
      next.set(id, nextState);
      return next;
    });
  }, [detail, sessionId]);

  // Leaving a session drops hydrate marks so a later re-select is fresh.
  useEffect(() => {
    if (!sessionId) {
      lastHydratedFp.current.clear();
      return;
    }
    for (const id of [...lastHydratedFp.current.keys()]) {
      if (id !== sessionId) lastHydratedFp.current.delete(id);
    }
  }, [sessionId]);

  const history = sessionId ? bySession.get(sessionId) : undefined;

  // Hydrate from disk page when ownership policy allows.
  useEffect(() => {
    if (!sessionId || !history) return;
    const fp = updatesPageFingerprint(history.updates);
    const prevFp = lastHydratedFp.current.get(sessionId);
    if (prevFp === fp) return;
    // After the first hydrate this visit, skip poll-driven resync while ACP owns the tail.
    if (prevFp != null && liveOwnsTail) return;
    lastHydratedFp.current.set(sessionId, fp);
    hydrateDiskLiveRef.current(sessionId, history.updates);
  }, [history, sessionId, liveOwnsTail]);

  const loadOlder = useCallback(async () => {
    if (
      !sessionId ||
      !history?.hasMore ||
      history.cursor == null ||
      loadingSessionId
    ) {
      return;
    }
    const requestedSessionId = sessionId;
    const requestedCursor = history.cursor;
    setLoadingSessionId(requestedSessionId);
    try {
      const page = await listSessionUpdates(
        requestedSessionId,
        requestedCursor,
        TIMELINE_PAGE_SIZE,
      );
      // Merge against the latest map entry (not a stale closure) so concurrent
      // detail polls cannot be overwritten, then hydrate that same snapshot.
      let mergedUpdates: unknown[] | null = null;
      setBySession((previous) => {
        const existing = previous.get(requestedSessionId);
        if (!existing || existing.cursor !== requestedCursor) return previous;
        const updates = mergeUpdatePages(page.updates, existing.updates);
        mergedUpdates = updates;
        const next = new Map(previous);
        next.set(requestedSessionId, {
          updates,
          cursor: page.nextCursor ?? null,
          hasMore: page.hasMore,
        });
        return next;
      });
      if (mergedUpdates) {
        lastHydratedFp.current.set(
          requestedSessionId,
          updatesPageFingerprint(mergedUpdates),
        );
        hydrateDiskLiveRef.current(requestedSessionId, mergedUpdates);
      }
    } finally {
      setLoadingSessionId((current) =>
        current === requestedSessionId ? null : current,
      );
    }
  }, [history, loadingSessionId, sessionId]);

  return useMemo(
    () => ({
      hasMore: history?.hasMore ?? false,
      loadingOlder: loadingSessionId === sessionId,
      loadOlder,
    }),
    [history?.hasMore, loadOlder, loadingSessionId, sessionId],
  );
}
