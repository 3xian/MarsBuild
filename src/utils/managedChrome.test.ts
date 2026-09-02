import { describe, expect, it } from "vitest";
import {
  isManagedTurnActive,
  isPinkcodeAttached,
  rankManagedCard,
  resolveCardState,
} from "./managedChrome";

describe("isPinkcodeAttached / isManagedTurnActive", () => {
  it("treats non-terminal statuses as attached", () => {
    expect(isPinkcodeAttached("ready")).toBe(true);
    expect(isPinkcodeAttached("starting")).toBe(true);
    expect(isPinkcodeAttached("running")).toBe(true);
    expect(isPinkcodeAttached("stopped")).toBe(false);
    expect(isPinkcodeAttached("error")).toBe(false);
    expect(isPinkcodeAttached(undefined)).toBe(false);
  });

  it("marks only mid-turn statuses as active", () => {
    expect(isManagedTurnActive("running")).toBe(true);
    expect(isManagedTurnActive("starting")).toBe(true);
    expect(isManagedTurnActive("ready")).toBe(false);
    expect(isManagedTurnActive("stopped")).toBe(false);
  });
});

describe("resolveCardState / rankManagedCard", () => {
  it("maps managed statuses to card chrome", () => {
    expect(resolveCardState("running", false)).toBe("running");
    expect(resolveCardState("stopping", false)).toBe("running");
    expect(resolveCardState("starting", false)).toBe("starting");
    expect(resolveCardState("awaitingPermission", false)).toBe("awaiting");
    expect(resolveCardState("ready", false)).toBe("live");
    expect(resolveCardState("error", true)).toBe("idle");
    expect(resolveCardState(undefined, true)).toBe("open");
    expect(resolveCardState(undefined, false)).toBe("idle");
  });

  it("needsInput projects to awaiting regardless of managed status", () => {
    expect(resolveCardState("ready", false, true)).toBe("awaiting");
    expect(resolveCardState("running", false, true)).toBe("awaiting");
    expect(resolveCardState(undefined, true, true)).toBe("awaiting");
  });

  it("ranks mid-turn above starting and open-elsewhere", () => {
    expect(rankManagedCard("running", false)).toBeLessThan(
      rankManagedCard("starting", false),
    );
    expect(rankManagedCard("starting", false)).toBeLessThan(
      rankManagedCard("ready", false),
    );
    expect(rankManagedCard("ready", false)).toBeLessThan(
      rankManagedCard(undefined, true),
    );
    expect(rankManagedCard(undefined, true)).toBeLessThan(
      rankManagedCard(undefined, false),
    );
  });

  it("ranks needsInput with awaitingPermission", () => {
    expect(rankManagedCard("ready", false, true)).toBe(
      rankManagedCard("awaitingPermission", false),
    );
  });
});
