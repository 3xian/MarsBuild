import type { ManagedAgentInfo } from "../types";

/** Agent is attached and can own the live ACP tail / timeline buffers. */
export function isLiveManagedStatus(
  status: ManagedAgentInfo["status"] | null | undefined,
): boolean {
  return (
    status === "ready" ||
    status === "running" ||
    status === "awaitingPermission"
  );
}

/**
 * Non-terminal attach, including `starting` / `stopping`.
 * Keep timeline buffers and skip disk page-1 resync while reconnecting.
 */
export function isAttachedManagedStatus(
  status: ManagedAgentInfo["status"] | null | undefined,
): boolean {
  return Boolean(status && status !== "stopped" && status !== "error");
}
