import { describe, expect, it } from "vitest";
import type { ManagedAgentInfo, TimelineItem } from "../types";
import {
  formatPhaseTimer,
  isManagedTurnActive,
  resolveTurnActivity,
  resolveTurnStartedAt,
  SEND_REFUSAL_HINT,
  sendRefusalActivity,
} from "./turnActivity";

function managed(
  status: ManagedAgentInfo["status"],
): ManagedAgentInfo {
  return {
    handleId: "h1",
    cwd: "/tmp",
    status,
    permissionMode: "default",
    alwaysApprove: false,
    createdAt: new Date().toISOString(),
  };
}

function item(
  partial: Partial<TimelineItem> & Pick<TimelineItem, "kind" | "title">,
): TimelineItem {
  return {
    id: partial.id ?? `${partial.kind}-${partial.title}`,
    handleId: "h1",
    ts: partial.ts ?? Date.now(),
    kind: partial.kind,
    title: partial.title,
    detail: partial.detail,
    streaming: partial.streaming,
    toolCallId: partial.toolCallId,
    toolBase: partial.toolBase,
    toolStatus: partial.toolStatus,
    shell: partial.shell,
  };
}

describe("formatPhaseTimer", () => {
  it("matches Grok Build format_duration buckets", () => {
    expect(formatPhaseTimer(500)).toBe("0.5s");
    expect(formatPhaseTimer(120)).toBe("0.1s");
    expect(formatPhaseTimer(5_200)).toBe("5.2s");
    expect(formatPhaseTimer(9_900)).toBe("9.9s");
    expect(formatPhaseTimer(10_000)).toBe("10s");
    expect(formatPhaseTimer(32_000)).toBe("32s");
    expect(formatPhaseTimer(80_000)).toBe("1m20s");
    expect(formatPhaseTimer(3_725_000)).toBe("1h2m");
  });
});

describe("isManagedTurnActive", () => {
  it("shows only active managed statuses", () => {
    expect(isManagedTurnActive("running")).toBe(true);
    expect(isManagedTurnActive("starting")).toBe(true);
    expect(isManagedTurnActive("awaitingPermission")).toBe(true);
    expect(isManagedTurnActive("stopping")).toBe(true);
    expect(isManagedTurnActive("ready")).toBe(false);
    expect(isManagedTurnActive("stopped")).toBe(false);
    expect(isManagedTurnActive(null)).toBe(false);
  });
});

describe("resolveTurnActivity", () => {
  it("maps managed lifecycle states", () => {
    expect(resolveTurnActivity(managed("starting"), [])?.label).toBe(
      "Starting session…",
    );
    expect(resolveTurnActivity(managed("stopping"), [])?.label).toBe(
      "Cancelling…",
    );
    expect(resolveTurnActivity(managed("awaitingPermission"), [])?.label).toBe(
      "Waiting for approval…",
    );
    expect(resolveTurnActivity(managed("ready"), [])).toBeNull();
  });

  it("reads thinking / responding / waiting from timeline tail while managed running", () => {
    const thinking = resolveTurnActivity(managed("running"), [
      item({ kind: "user", title: "User", detail: "hi" }),
      item({ kind: "thought", title: "Thinking", streaming: true }),
    ]);
    expect(thinking?.label).toBe("Thinking…");
    expect(thinking?.kind).toBe("thinking");
    expect(thinking?.source).toBe("managed");

    const responding = resolveTurnActivity(managed("running"), [
      item({ kind: "user", title: "User" }),
      item({ kind: "agent", title: "Agent", streaming: true, detail: "Hello" }),
    ]);
    expect(responding?.label).toBe("Responding…");

    const waiting = resolveTurnActivity(managed("running"), [
      item({ kind: "user", title: "User" }),
    ]);
    expect(waiting?.label).toBe("Waiting for response…");
  });

  it("formats active tools as Run + title", () => {
    const act = resolveTurnActivity(managed("running"), [
      item({
        kind: "tool",
        title: "read_file · pending",
        toolBase: "read_file",
        toolStatus: "pending",
        toolCallId: "tc1",
      }),
    ]);
    expect(act?.label).toBe("Run ");
    expect(act?.detail).toBe("read_file");
    expect(act?.tone).toBe("tool");
  });

  it("uses shell description when present", () => {
    const act = resolveTurnActivity(managed("running"), [
      item({
        kind: "shell",
        title: "$ sleep 5",
        shell: {
          toolCallId: "s1",
          command: "sleep 5",
          description: "Wait briefly",
          status: "in_progress",
          output: "",
        },
      }),
    ]);
    expect(act?.label).toBe("Wait briefly…");
    expect(act?.tone).toBe("muted");
  });

  it("falls back to Waiting after a completed tool while still running", () => {
    const act = resolveTurnActivity(managed("running"), [
      item({
        kind: "tool",
        title: "read_file ✓",
        toolBase: "read_file",
        toolStatus: "completed",
        toolCallId: "tc1",
      }),
    ]);
    expect(act?.label).toBe("Waiting for response…");
  });

  it("shows ambient status when session is open externally (not PinkCode mid-turn)", () => {
    const act = resolveTurnActivity(null, [], { sessionIsActive: true });
    expect(act?.kind).toBe("external");
    expect(act?.label).toBe("Open in Grok Build");
    expect(act?.source).toBe("external");
    expect(act?.hint).toMatch(/connect/i);
  });

  it("paints a send refusal like Disconnected (danger, on the composer)", () => {
    const act = sendRefusalActivity(SEND_REFUSAL_HINT.openElsewhere);
    expect(act.label).toBe("Not sent");
    expect(act.tone).toBe("danger");
    expect(act.indicator).toBe("still");
    expect(act.hint).toBe("Already open in Grok Build");
    expect(act.showPhaseTimer).toBe(false);
  });

  it("does not call a PinkCode ACP error Open in Grok Build", () => {
    const act = resolveTurnActivity(managed("error"), [], {
      sessionIsActive: true,
    });
    expect(act?.label).toBe("Disconnected");
    expect(act?.source).toBe("managed");
    expect(act?.kind).toBe("waiting");
  });

  it("does not show external ambient when PinkCode is already connected idle", () => {
    expect(
      resolveTurnActivity(managed("ready"), [], { sessionIsActive: true }),
    ).toBeNull();
  });

  it("infers in-flight tool from disk timeline for external open session", () => {
    const act = resolveTurnActivity(
      null,
      [
        item({
          kind: "tool",
          title: "read_file · pending",
          toolBase: "read_file",
          toolStatus: "pending",
          toolCallId: "tc-ext",
        }),
      ],
      { sessionIsActive: true },
    );
    expect(act?.label).toBe("Run ");
    expect(act?.detail).toBe("read_file");
    expect(act?.source).toBe("external");
    expect(act?.hint).toMatch(/Grok Build/i);
  });

  it("stays ambient external when timeline only has finished work", () => {
    const act = resolveTurnActivity(
      null,
      [
        item({ kind: "user", title: "User" }),
        item({ kind: "agent", title: "Agent", detail: "done" }),
        item({ kind: "event", title: "Worked for 3.0s" }),
      ],
      { sessionIsActive: true },
    );
    expect(act?.kind).toBe("external");
    expect(act?.label).toBe("Open in Grok Build");
  });

  it("does not treat status-less tools as in-flight for external hydrate", () => {
    // Disk timeline often omits toolStatus on settled rows.
    const act = resolveTurnActivity(
      null,
      [
        item({
          kind: "tool",
          title: "read_file",
          toolBase: "read_file",
          toolCallId: "tc-hist",
        }),
      ],
      { sessionIsActive: true },
    );
    expect(act?.kind).toBe("external");
    expect(act?.label).toBe("Open in Grok Build");
  });

  it("still treats status-less tools as in-flight while managed is running", () => {
    const act = resolveTurnActivity(managed("running"), [
      item({
        kind: "tool",
        title: "read_file",
        toolBase: "read_file",
        toolCallId: "tc-live",
      }),
    ]);
    expect(act?.label).toBe("Run ");
    expect(act?.detail).toBe("read_file");
    expect(act?.source).toBe("managed");
  });
});

describe("resolveTurnStartedAt", () => {
  it("uses the last user card timestamp", () => {
    const t0 = 1_000_000;
    const t1 = 1_005_000;
    const now = 1_010_000;
    const start = resolveTurnStartedAt(
      [
        item({ kind: "user", title: "User", ts: t0 }),
        item({ kind: "agent", title: "Agent", ts: t1 }),
      ],
      now,
    );
    expect(start).toBe(t0);
  });
});
