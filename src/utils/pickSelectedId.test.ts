import { describe, expect, it } from "vitest";
import type { ManagedAgentInfo, SessionCard } from "../types";
import { pickSelectedId } from "./pickSelectedId";

function card(id: string, isActive = false): SessionCard {
  return {
    id,
    cwd: "/tmp",
    title: id,
    numMessages: 0,
    isActive,
    status: "idle",
    contextTokensUsed: 0,
    contextWindowTokens: 0,
    contextWindowUsage: 0,
    totalTokens: 0,
    tokenUsageIncomplete: false,
    tokenUsageAvailable: false,
    tokenUsagePending: false,
    toolCallCount: 0,
    turnCount: 0,
    toolsUsed: [],
    agentLinesAdded: 0,
    agentLinesRemoved: 0,
    agentFilesTouched: 0,
    sessionDurationSeconds: 0,
    errorCount: 0,
  };
}

function managed(sessionId: string): ManagedAgentInfo {
  return {
    handleId: "h1",
    sessionId,
    cwd: "/tmp",
    status: "ready",
    permissionMode: "default",
    alwaysApprove: false,
    createdAt: "2026-01-01T00:00:00Z",
  };
}

describe("pickSelectedId", () => {
  const page = [card("a"), card("b"), card("c", true)];

  it("keeps prev when it is on this page", () => {
    expect(pickSelectedId(page, "b")).toBe("b");
    expect(pickSelectedId(page, "b", { prevOnDisk: false })).toBe("b");
  });

  it("keeps prev when it is off this page but still on disk", () => {
    expect(pickSelectedId(page, "off-page", { prevOnDisk: true })).toBe(
      "off-page",
    );
  });

  it("drops prev when it is absent from disk", () => {
    expect(pickSelectedId(page, "gone", { prevOnDisk: false })).toBe("c");
  });

  it("drops off-page prev when disk presence was not confirmed", () => {
    expect(pickSelectedId(page, "unknown")).toBe("c");
  });

  it("falls back to managed, then live, then first card", () => {
    const idle = [card("a"), card("b")];
    expect(
      pickSelectedId(idle, null, { managed: [managed("b")] }),
    ).toBe("b");
    expect(pickSelectedId(page, null)).toBe("c");
    expect(pickSelectedId(idle, null)).toBe("a");
    expect(pickSelectedId([], null)).toBe(null);
  });
});
