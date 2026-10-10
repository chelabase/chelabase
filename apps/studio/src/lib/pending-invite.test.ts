import { afterEach, describe, expect, it, vi } from "vitest";
import { peekPendingInvite, resumeInvite, savePendingInvite, takePendingInvite } from "./pending-invite";

afterEach(() => {
  window.sessionStorage.clear();
  vi.useRealTimers();
});

describe("pending invite", () => {
  it("round_trips_once_for_the_same_user", () => {
    savePendingInvite({ user: "z1", code: "c1", t: "tok", reset: false });
    expect(peekPendingInvite()).toEqual({ user: "z1" });
    expect(takePendingInvite("z2")).toBeNull();
    expect(takePendingInvite("z1")).toEqual({ user: "z1", code: "c1", t: "tok", reset: false });
    // Taken: gone, so a later sign-in is never sent to the accept page.
    expect(peekPendingInvite()).toBeNull();
    expect(takePendingInvite("z1")).toBeNull();
  });

  it("expires_after_ten_minutes", () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-09T10:00:00Z"));
    savePendingInvite({ user: "z1", code: "c1", t: "", reset: true });
    vi.setSystemTime(new Date("2026-10-09T10:10:01Z"));
    expect(peekPendingInvite()).toBeNull();
    expect(takePendingInvite("z1")).toBeNull();
  });

  it("login_sends_a_pending_invite_back_to_the_accept_page", () => {
    expect(resumeInvite("V2_1")).toBeNull();
    savePendingInvite({ user: "z1", code: "c1", t: "tok", reset: false });
    // No pending sign-in: nothing to finish, the sign-in page stays.
    expect(resumeInvite("")).toBeNull();
    // Only the user and the pending sign-in: the code stays in this tab's storage.
    expect(resumeInvite("V2_1")).toEqual({ user: "z1", authRequest: "V2_1" });
  });

  it("ignores_garbage", () => {
    window.sessionStorage.setItem("chelabase.pending-invite", "{not json");
    expect(peekPendingInvite()).toBeNull();
    window.sessionStorage.setItem("chelabase.pending-invite", JSON.stringify({ user: 1 }));
    expect(peekPendingInvite()).toBeNull();
  });
});
