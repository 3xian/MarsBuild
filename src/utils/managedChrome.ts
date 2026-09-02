import type { ManagedStatus } from "../types";
import { isAttachedManagedStatus } from "./managedStatus";

/**
 * Visual run-state for a task card / list chrome.
 * - running: PinkCode ACP mid-turn (or stopping)
 * - starting: spawn/connect in flight
 * - open: Grok Build (or other host) process still open — not mid-turn here
 * - awaiting / live: PinkCode attached
 * - awaiting also covers open reverse-requests (permission / question / plan)
 */
export type CardState =
  | "running"
  | "starting"
  | "awaiting"
  | "live"
  | "open"
  | "idle";

/** Non-terminal PinkCode attach (includes starting / ready / running / …). */
export function isPinkcodeAttached(st?: ManagedStatus | null): boolean {
  return isAttachedManagedStatus(st);
}

const MANAGED_ACTIVE = new Set<ManagedStatus>([
  "starting",
  "running",
  "awaitingPermission",
  "stopping",
]);

/** PinkCode ACP mid-turn (or starting / stopping / awaiting). */
export function isManagedTurnActive(status?: ManagedStatus | null): boolean {
  return Boolean(status && MANAGED_ACTIVE.has(status));
}

/**
 * @param needsInput — open reverse-request (permission queue or pending_interaction).
 *   Single projection for "Needs input"; do not special-case in list components.
 */
export function resolveCardState(
  managedStatus: ManagedStatus | undefined,
  openElsewhere: boolean,
  needsInput = false,
): CardState {
  // Reverse-request wins over live chrome so the roster surfaces Needs input.
  if (needsInput) return "awaiting";
  const st = managedStatus;
  if (st === "running" || st === "stopping") return "running";
  if (st === "starting") return "starting";
  if (st === "awaitingPermission") return "awaiting";
  if (isPinkcodeAttached(st)) return "live";
  if (st === "error") return "idle";
  if (openElsewhere) return "open";
  return "idle";
}

/**
 * Sort rank: lower = higher in list.
 * `starting` ranks below mid-turn so the list does not jump while connecting.
 */
export function rankManagedCard(
  managedStatus: ManagedStatus | undefined,
  sessionIsActive: boolean,
  needsInput = false,
): number {
  if (needsInput) return 1;
  const st = managedStatus;
  if (st === "running" || st === "stopping") return 0;
  if (st === "awaitingPermission") return 1;
  if (st === "starting") return 2;
  if (isPinkcodeAttached(st)) return 3;
  if (sessionIsActive) return 4;
  return 5;
}

export function stateLabel(state: CardState): string {
  switch (state) {
    case "running":
      return "Running";
    case "starting":
      return "Starting";
    case "open":
      return "Open";
    case "awaiting":
      return "Needs input";
    case "live":
      return "Live";
    case "idle":
      return "Idle";
  }
}

export function stateTitle(state: CardState): string {
  switch (state) {
    case "running":
      return "Agent is working on this task in PinkCode";
    case "starting":
      return "Connecting…";
    case "open":
      return "Open in Grok Build — send a message here to connect";
    case "awaiting":
      return "Waiting for your input (permission / question / plan)";
    case "live":
      return "Connected in PinkCode";
    case "idle":
      return "Idle";
  }
}
