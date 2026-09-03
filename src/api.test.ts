import { describe, expect, it } from "vitest";
import { formatInvokeError, isExclusiveSessionError } from "./api";

describe("isExclusiveSessionError", () => {
  it("matches a typed attach reject payload", () => {
    expect(
      isExclusiveSessionError({
        code: "session_open_elsewhere",
        pid: 4321,
        message: "session is already open in another Grok process (pid 4321)",
      }),
    ).toBe(true);
  });

  it("matches a nested code field", () => {
    expect(
      isExclusiveSessionError({
        error: { code: "session_open_elsewhere", message: "busy" },
      }),
    ).toBe(true);
  });

  it("does not match a message-only reject", () => {
    expect(
      isExclusiveSessionError(
        "session is already open in another Grok process (pid 1)",
      ),
    ).toBe(false);
    expect(isExclusiveSessionError({ message: "boom" })).toBe(false);
  });
});

describe("formatInvokeError", () => {
  it("prefers message on a typed payload", () => {
    expect(
      formatInvokeError({
        code: "session_open_elsewhere",
        message: "session is already open in another Grok process (pid 9)",
      }),
    ).toBe("session is already open in another Grok process (pid 9)");
  });
});
