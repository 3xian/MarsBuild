import type { ManagedAgentInfo, SessionCard } from "../types";

export type PickSelectedIdOptions = {
  managed?: ManagedAgentInfo[];
  /**
   * When `prev` is missing from this roster page: true if the task still
   * exists on disk, false if it is gone. Omit only when `prev` is in `list`.
   */
  prevOnDisk?: boolean;
};

/**
 * Single selection policy for a paged roster.
 * Keep `prev` when it is on this page, or off-page but still on disk.
 * Drop it when it is absent from disk so a deleted task cannot stuck-load.
 */
export function pickSelectedId(
  list: SessionCard[],
  prev: string | null,
  options?: PickSelectedIdOptions,
): string | null {
  if (prev && list.some((s) => s.id === prev)) return prev;
  if (prev && options?.prevOnDisk) return prev;
  if (options?.managed?.length) {
    const managedSid = options.managed.find((m) => m.sessionId)?.sessionId;
    if (managedSid && list.some((s) => s.id === managedSid)) {
      return managedSid;
    }
  }
  const live = list.find((s) => s.isActive);
  return live?.id ?? list[0]?.id ?? null;
}
