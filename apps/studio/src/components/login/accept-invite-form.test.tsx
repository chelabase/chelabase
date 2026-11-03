import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactElement } from "react";
import { describe, expect, it, vi } from "vitest";
import { configureLoginClient } from "@/lib/login-client/fetch";
import { fakeFetch, jsonResponse, sequence, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { AcceptInviteForm } from "./accept-invite-form";

const DONE = { next: "done", redirect: "http://studio.chelabase.localhost:8000/app" };
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

async function submit(password: string, button = "Set password and sign in") {
  const field = await screen.findByLabelText("Password");
  await userEvent.clear(field);
  await userEvent.type(field, password);
  await userEvent.click(screen.getByRole("button", { name: button }));
}

describe("AcceptInviteForm", () => {
  it("sets_password_and_signs_in", async () => {
    const go = vi.fn();
    const { f } = renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={go} />, {
      "POST /auth/v1/invite": () => jsonResponse({ inviter_name: "Ana", inviter_email: "ana@example.test", tenant_name: "Acme" }),
      "POST /auth/v1/accept-invite": () => jsonResponse(DONE),
    });
    expect(await screen.findByRole("heading", { name: "Join Chelabase" })).toBeInTheDocument();
    expect(screen.getByLabelText("Password")).toHaveFocus();
    expect(await screen.findByText(/Ana/)).toHaveTextContent("Ana (ana@example.test) invited you to Acme.");
    await submit("Secret-pass1");
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://studio.chelabase.localhost:8000/app"));
    expect(await bodies(f)).toEqual([{ user: "z1", code: "c1", password: "Secret-pass1" }]);
    // M8: the user and the token go in a POST body, never in the URL.
    const describe = f.calls.find((r) => new URL(r.url).pathname === "/auth/v1/invite")!;
    expect(describe.method).toBe("POST");
    expect(new URL(describe.url).search).toBe("");
    expect(await describe.clone().json()).toEqual({ user: "z1", t: "tok" });
  });

  it("sends_reset_for_a_password_reset_code", async () => {
    // A removed member invited again gets a password-reset code (the link's `reset=1`).
    const go = vi.fn();
    const { f } = renderAccept(<AcceptInviteForm user="z1" code="c1" t="" reset go={go} />, {
      "POST /auth/v1/accept-invite": () => jsonResponse(DONE),
    });
    await submit("Secret-pass1");
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://studio.chelabase.localhost:8000/app"));
    expect(await bodies(f)).toEqual([{ user: "z1", code: "c1", reset: true, password: "Secret-pass1" }]);
  });

  it("shows_expired_and_revoked", async () => {
    const first = renderAccept(<AcceptInviteForm user="z1" code="c1" t="" go={vi.fn()} />, {
      "POST /auth/v1/accept-invite": err("invite_expired", "expired", 410),
    });
    await submit("Secret-pass1");
    expect(await screen.findByRole("heading", { name: "This invite has expired" })).toHaveFocus();
    expect(screen.queryByLabelText("Password")).toBeNull();
    // No `t`: the inviter lookup never runs.
    expect(first.f.calls.some((r) => new URL(r.url).pathname === "/auth/v1/invite")).toBe(false);
    first.unmount();

    renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/accept-invite": err("invite_revoked", "revoked", 410),
    });
    await submit("Secret-pass1");
    expect(await screen.findByRole("heading", { name: "This invite was withdrawn" })).toBeInTheDocument();
  });

  it("an incomplete link shows no form", () => {
    renderAccept(<AcceptInviteForm user="z1" code="" t="tok" go={vi.fn()} />);
    expect(screen.getByRole("heading", { name: "This invite link is incomplete" })).toBeInTheDocument();
    expect(screen.queryByLabelText("Password")).toBeNull();
  });

  it("retries_with_accept_ticket", async () => {
    const go = vi.fn();
    const { f } = renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={go} />, {
      "POST /auth/v1/accept-invite": sequence(
        err("weak_password", "Password must have 12 characters", 400, { accept_ticket: "tk1" }),
        err("invite_incomplete", "setting up the account didn't finish", 503, { accept_ticket: "tk2" }),
        () => jsonResponse(DONE),
      ),
    });
    await submit("short");
    expect(await screen.findByRole("alert")).toHaveTextContent("Password must have 12 characters");
    expect(screen.getByLabelText("Password")).toHaveValue("");
    expect(screen.getByLabelText("Password")).toHaveFocus();
    await submit("Secret-pass1");
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Your invite code is confirmed, but setting up your account didn't finish. Try again.",
    );
    // The password is kept: "Try again" resubmits with the newest ticket.
    await userEvent.click(screen.getByRole("button", { name: "Try again" }));
    await waitFor(() => expect(go).toHaveBeenCalledWith("http://studio.chelabase.localhost:8000/app"));
    expect(await bodies(f)).toEqual([
      { user: "z1", code: "c1", password: "short" },
      { user: "z1", acceptTicket: "tk1", password: "Secret-pass1" },
      { user: "z1", acceptTicket: "tk2", password: "Secret-pass1" },
    ]);
  });

  it("a spent code without a ticket says to start again, and never resends the code", async () => {
    for (const code of ["invite_incomplete", "weak_password"]) {
      const r = renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={vi.fn()} />, {
        "POST /auth/v1/accept-invite": err(code, "no ticket", code === "weak_password" ? 400 : 503),
      });
      await submit("Secret-pass1");
      expect(await screen.findByRole("heading", { name: "Setting up your account didn't finish" })).toBeInTheDocument();
      expect(screen.getByText(/Open the link from your invite email again, or ask the person who invited you for a new one/)).toBeInTheDocument();
      expect(screen.queryByLabelText("Password")).toBeNull();
      expect((await bodies(r.f)).length).toBe(1);
      r.unmount();
    }
  });

  it("a suspended member is told why, not asked to retry", async () => {
    const r = renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/accept-invite": err("member_suspended", "suspended", 403),
    });
    await submit("Secret-pass1");
    expect(await screen.findByText(/your access is suspended/i)).toBeInTheDocument();
    expect(screen.getByText(/sign in with the password you just set/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /try again/i })).toBeNull();
    expect((await bodies(r.f)).length).toBe(1);
  });

  // M8: the code and token leave the address bar (and so the history) as soon as the page has
  // read them, not at the first submit; the page keeps working from what it read.
  it("the code and token leave the address bar on mount and never enter a query key", async () => {
    window.history.replaceState(null, "", "/accept-invite?user=z1&code=c1&t=tok");
    const go = vi.fn();
    const { queryClient, rerender, f } = renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={go} />, {
      "POST /auth/v1/invite": () => jsonResponse({ inviter_name: "Ana", inviter_email: null, tenant_name: "Acme" }),
      "POST /auth/v1/accept-invite": () => jsonResponse(DONE),
    });
    expect(window.location.search).toBe("?user=z1");
    await screen.findByText(/invited you to Acme/);
    expect(JSON.stringify(queryClient.getQueryCache().getAll().map((q) => q.queryKey))).not.toContain("tok");
    // The router re-renders with the new address: the page does not forget the link.
    rerender(<AcceptInviteForm user="z1" code="" t="" go={go} />);
    expect(screen.getByLabelText("Password")).toBeInTheDocument();
    await submit("Secret-pass1");
    await waitFor(() => expect(go).toHaveBeenCalled());
    expect(await bodies(f)).toEqual([{ user: "z1", code: "c1", password: "Secret-pass1" }]);
    expect(window.location.search).toBe("?user=z1");
  });

  it("a_closed_invite_shows_before_the_password", async () => {
    // describe-invite answers 404 for an expired, withdrawn or used invite: no password is asked
    // for and no sign-in starts.
    renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/invite": err("not_found", "not found", 404),
    });
    expect(await screen.findByRole("heading", { name: "This invite link no longer works" })).toHaveFocus();
    expect(screen.getByText(/It has expired, been withdrawn or already been used/)).toBeInTheDocument();
    expect(screen.queryByLabelText("Password")).toBeNull();
  });

  it("a_failed_lookup_never_blocks_the_invite", async () => {
    renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={vi.fn()} />, {
      "POST /auth/v1/invite": err("unavailable", "down", 503),
    });
    expect(await screen.findByLabelText("Password")).toBeInTheDocument();
  });

  it("the_form_waits_for_the_inviter_lookup_and_needs_no_sign_in_first", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={vi.fn()} />, {
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
    renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={vi.fn()} />, {
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
    renderAccept(<AcceptInviteForm user="z1" code="c1" t="tok" go={vi.fn()} />, {
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
