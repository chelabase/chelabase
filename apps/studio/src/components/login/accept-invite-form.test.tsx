import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { describe, expect, it, vi } from "vitest";
import { configureLoginClient } from "@/lib/login-client/fetch";
import { fakeFetch, jsonResponse, sequence, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { AcceptInviteForm } from "./accept-invite-form";

const STUDIO = { next: "studio" };
const err = (code: string, message: string, status: number, extra: Record<string, unknown> = {}) => () =>
  jsonResponse({ error: { code, message, ...extra } }, status);

function renderAccept(ui: ReactElement, routes: Record<string, FakeHandler> = {}) {
  const f = fakeFetch({
    // An open invite unless a test says otherwise.
    "POST /auth/v1/invite": () => jsonResponse({ inviter_name: "Ana", inviter_email: null, tenant_name: "Acme" }),
    ...routes,
  });
  configureLoginClient({ baseUrl: "http://studio.test", fetchImpl: f });
  return { ...renderWithQuery(ui, { fetch: f }), f };
}

const bodies = (f: { calls: Request[] }) =>
  Promise.all(f.calls.filter((r) => new URL(r.url).pathname === "/auth/v1/accept-invite").map((r) => r.clone().json()));

const describeCalls = (f: { calls: Request[] }) => f.calls.filter((r) => new URL(r.url).pathname === "/auth/v1/invite");

async function submit(password: string, button = "Set password and sign in") {
  const field = await screen.findByLabelText("Password");
  await userEvent.clear(field);
  await userEvent.type(field, password);
  await userEvent.click(screen.getByRole("button", { name: button }));
}

describe("AcceptInviteForm", () => {
  it("a link with its code asks only for a password, and signs in", async () => {
    const go = vi.fn();
    const { f } = renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={go} />, {
      "POST /auth/v1/invite": () => jsonResponse({ inviter_name: "Ana", inviter_email: "ana@example.test", tenant_name: "Acme" }),
      "POST /auth/v1/accept-invite": () => jsonResponse(STUDIO),
    });
    expect(await screen.findByRole("heading", { name: "Join Chelabase" })).toBeInTheDocument();
    expect(screen.getByLabelText("Password")).toHaveFocus();
    expect(screen.queryByLabelText("Invite code")).toBeNull();
    expect(await screen.findByText(/Ana/)).toHaveTextContent("Ana (ana@example.test) invited you to Acme.");
    await submit("Secret-pass1");
    await waitFor(() => expect(go).toHaveBeenCalledWith("/app"));
    expect(await bodies(f)).toEqual([{ user: "u1", t: "tok", code: "123456", password: "Secret-pass1" }]);
    // M8: the user and the token go in a POST body, never in the URL.
    const describe = describeCalls(f)[0]!;
    expect(describe.method).toBe("POST");
    expect(new URL(describe.url).search).toBe("");
    expect(await describe.clone().json()).toEqual({ user: "u1", t: "tok" });
  });

  it("an emailed link asks for the code from the email", async () => {
    const go = vi.fn();
    const { f } = renderAccept(<AcceptInviteForm user="u1" code="" t="tok" go={go} />, {
      "POST /auth/v1/accept-invite": () => jsonResponse(STUDIO),
    });
    await userEvent.type(await screen.findByLabelText("Invite code"), " 654321 ");
    await submit("Secret-pass1");
    await waitFor(() => expect(go).toHaveBeenCalledWith("/app"));
    expect(await bodies(f)).toEqual([{ user: "u1", t: "tok", code: "654321", password: "Secret-pass1" }]);
  });

  it("an existing account joins by signing in, with no inviter lookup and no new password", async () => {
    const go = vi.fn();
    const { f } = renderAccept(<AcceptInviteForm user="" code="" t="tok" go={go} />, {
      "POST /auth/v1/accept-invite": sequence(
        err("invalid_credentials", "Email or password is incorrect", 401),
        () => jsonResponse(STUDIO),
      ),
    });
    expect(await screen.findByRole("heading", { name: "Sign in to accept your invite" })).toBeInTheDocument();
    await userEvent.type(screen.getByLabelText("Email"), "ann@example.com");
    await submit("Wrong-pass1", "Sign in and accept");
    expect(await screen.findByRole("alert")).toHaveTextContent("Email or password is incorrect");
    await submit("Own-pass-12", "Sign in and accept");
    await waitFor(() => expect(go).toHaveBeenCalledWith("/app"));
    expect(await bodies(f)).toEqual([
      { t: "tok", email: "ann@example.com", password: "Wrong-pass1" },
      { t: "tok", email: "ann@example.com", password: "Own-pass-12" },
    ]);
    expect(describeCalls(f)).toHaveLength(0);
  });

  it("a member waiting for approval goes to the waiting screen", async () => {
    const go = vi.fn();
    renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={go} />, {
      "POST /auth/v1/accept-invite": () => jsonResponse({ next: "pending" }),
    });
    await submit("Secret-pass1");
    await waitFor(() => expect(go).toHaveBeenCalledWith("/pending"));
  });

  it("shows_expired_and_revoked", async () => {
    const first = renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/accept-invite": err("invite_expired", "expired", 410),
    });
    await submit("Secret-pass1");
    expect(await screen.findByRole("heading", { name: "This invite has expired" })).toHaveFocus();
    expect(screen.queryByLabelText("Password")).toBeNull();
    first.unmount();

    renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/accept-invite": err("invite_revoked", "revoked", 410),
    });
    await submit("Secret-pass1");
    expect(await screen.findByRole("heading", { name: "This invite was withdrawn" })).toBeInTheDocument();
  });

  it("an incomplete link shows no form", () => {
    for (const [user, code] of [["u1", "123456"], ["", "123456"]]) {
      const r = renderAccept(<AcceptInviteForm user={user!} code={code!} t="" go={vi.fn()} />);
      expect(screen.getByRole("heading", { name: "This invite link is incomplete" })).toBeInTheDocument();
      expect(screen.queryByLabelText("Password")).toBeNull();
      r.unmount();
    }
  });

  it("a weak password keeps the code: the same request again", async () => {
    const go = vi.fn();
    const { f } = renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={go} />, {
      "POST /auth/v1/accept-invite": sequence(err("weak_password", "Use at least 8 characters", 400), () => jsonResponse(STUDIO)),
    });
    await submit("short");
    expect(await screen.findByRole("alert")).toHaveTextContent("Use at least 8 characters");
    expect(screen.getByLabelText("Password")).toHaveValue("");
    expect(screen.getByLabelText("Password")).toHaveFocus();
    await submit("Secret-pass1");
    await waitFor(() => expect(go).toHaveBeenCalledWith("/app"));
    expect(await bodies(f)).toEqual([
      { user: "u1", t: "tok", code: "123456", password: "short" },
      { user: "u1", t: "tok", code: "123456", password: "Secret-pass1" },
    ]);
  });

  it("retries_with_accept_ticket", async () => {
    const go = vi.fn();
    const { f } = renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={go} />, {
      "POST /auth/v1/accept-invite": sequence(
        err("invite_incomplete", "setting up the account didn't finish", 503, { accept_ticket: "tk1" }),
        err("invite_incomplete", "setting up the account didn't finish", 503, { accept_ticket: "tk2" }),
        () => jsonResponse(STUDIO),
      ),
    });
    await submit("Secret-pass1");
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Your invite code is confirmed, but setting up your account didn't finish. Try again.",
    );
    // The password is kept: "Try again" resubmits with the newest ticket.
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    await screen.findByRole("button", { name: "Try again" });
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(go).toHaveBeenCalledWith("/app"));
    expect(await bodies(f)).toEqual([
      { user: "u1", t: "tok", code: "123456", password: "Secret-pass1" },
      { user: "u1", t: "tok", acceptTicket: "tk1", password: "Secret-pass1" },
      { user: "u1", t: "tok", acceptTicket: "tk2", password: "Secret-pass1" },
    ]);
  });

  it("a spent code without a ticket says to start again, and never resends the code", async () => {
    const r = renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/accept-invite": err("invite_incomplete", "no ticket", 503),
    });
    await submit("Secret-pass1");
    expect(await screen.findByRole("heading", { name: "Setting up your account didn't finish" })).toBeInTheDocument();
    expect(screen.getByText(/Open the link from your invite email again, or ask the person who invited you for a new one/)).toBeInTheDocument();
    expect(screen.queryByLabelText("Password")).toBeNull();
    expect((await bodies(r.f)).length).toBe(1);
  });

  it("a suspended member is told why, not asked to retry", async () => {
    const r = renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/accept-invite": err("member_suspended", "suspended", 403),
    });
    await submit("Secret-pass1");
    expect(await screen.findByText(/your access is suspended/i)).toBeInTheDocument();
    expect(screen.getByText(/ask an admin to reinstate you/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /try again/i })).toBeNull();
    expect((await bodies(r.f)).length).toBe(1);
  });

  // M8: the code and token leave the address bar (and so the history) as soon as the page has
  // read them, not at the first submit; the page keeps working from what it read.
  it("the code and token leave the address bar on mount and never enter a query key", async () => {
    window.history.replaceState(null, "", "/accept-invite?user=u1&code=123456&t=tok");
    const go = vi.fn();
    const { queryClient, rerender, f } = renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={go} />, {
      "POST /auth/v1/accept-invite": () => jsonResponse(STUDIO),
    });
    expect(window.location.search).toBe("?user=u1");
    await screen.findByText(/invited you to Acme/);
    expect(JSON.stringify(queryClient.getQueryCache().getAll().map((q) => q.queryKey))).not.toContain("tok");
    // The router re-renders with the new address: the page does not forget the link.
    rerender(<AcceptInviteForm user="u1" code="" t="" go={go} />);
    expect(screen.getByLabelText("Password")).toBeInTheDocument();
    expect(screen.queryByLabelText("Invite code")).toBeNull();
    await submit("Secret-pass1");
    await waitFor(() => expect(go).toHaveBeenCalled());
    expect(await bodies(f)).toEqual([{ user: "u1", t: "tok", code: "123456", password: "Secret-pass1" }]);
    expect(window.location.search).toBe("?user=u1");
  });

  it("a notice link's token leaves the address bar too", async () => {
    window.history.replaceState(null, "", "/accept-invite?t=tok");
    renderAccept(<AcceptInviteForm user="" code="" t="tok" go={vi.fn()} />);
    expect(window.location.search).toBe("");
    expect(await screen.findByLabelText("Email")).toBeInTheDocument();
  });

  it("a_closed_invite_shows_before_the_password", async () => {
    // describe-invite answers 404 for an expired, withdrawn or used invite: no password is asked
    // for and no sign-in starts.
    renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/invite": err("not_found", "not found", 404),
    });
    expect(await screen.findByRole("heading", { name: "This invite link no longer works" })).toHaveFocus();
    expect(screen.getByText(/It has expired, been withdrawn or already been used/)).toBeInTheDocument();
    expect(screen.queryByLabelText("Password")).toBeNull();
  });

  it("a_failed_lookup_never_blocks_the_invite", async () => {
    renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/invite": err("unavailable", "down", 503),
    });
    expect(await screen.findByLabelText("Password")).toBeInTheDocument();
  });

  it("the_form_waits_for_the_inviter_lookup_and_needs_no_sign_in_first", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/invite": async () => {
        await gate;
        return jsonResponse({ inviter_name: "Ana", inviter_email: null, tenant_name: "Acme" });
      },
    });
    expect(screen.getByText("Opening your invite…")).toBeInTheDocument();
    expect(screen.queryByLabelText("Password")).toBeNull();
    release();
    expect(await screen.findByLabelText("Password")).toBeInTheDocument();
  });

  it("a spent ticket ends at the sign-in advice", async () => {
    renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/accept-invite": sequence(
        err("invite_incomplete", "unfinished", 503, { accept_ticket: "tk1" }),
        err("invalid_request", "bad ticket", 400),
      ),
    });
    await submit("Secret-pass1");
    await userEvent.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { name: "This invite link doesn't work" })).toBeInTheDocument();
    expect(screen.getByText(/Setting up your account didn't finish in time/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Sign in" })).toHaveAttribute("href", "/app");
  });

  it("focuses the ending heading in the same commit that inserts it", async () => {
    // The flake (T2): focus came from a passive effect, a task after the commit, so an assertion
    // that ran first under load saw <body>. The first microtask after the insertion is
    // deterministic: after the commit, before any passive effect.
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    renderAccept(<AcceptInviteForm user="u1" code="123456" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/invite": async () => {
        await gate;
        return jsonResponse({ error: { code: "not_found", message: "nf" } }, 404);
      },
    });
    let atInsert: Element | null | undefined;
    const obs = new MutationObserver(() => {
      if (atInsert === undefined && screen.queryByRole("heading", { name: "This invite link no longer works" })) {
        atInsert = document.activeElement;
      }
    });
    obs.observe(document.body, { childList: true, subtree: true });
    release();
    const heading = await screen.findByRole("heading", { name: "This invite link no longer works" });
    obs.disconnect();
    expect(atInsert).toBe(heading);
  });
});
