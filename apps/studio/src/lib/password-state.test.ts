import { describe, expect, it } from "vitest";
import { PASSWORD_STATE_TEXT } from "./password-state";

describe("PASSWORD_STATE_TEXT", () => {
  it("password_state_unreadable_text", () => {
    expect(PASSWORD_STATE_TEXT.unreadable).toBe("Stored password can't be read; reset it.");
  });
});
