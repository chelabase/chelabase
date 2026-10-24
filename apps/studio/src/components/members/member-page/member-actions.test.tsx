import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Member } from "@chelabase/management-client";
import { MARA, SOFIA, apiError, calls, mount, ok, sofiaFor, LOGIN_LATER, LOGIN_REVOKED, LOGIN_SOON, type Caller } from "./test-setup";

const SUSPEND = `POST /v1/members/${SOFIA}/suspend`;
const REINSTATE = `POST /v1/members/${SOFIA}/reinstate`;
const REVOKE = `POST /v1/members/${SOFIA}/revoke-postgres-access`;

/** A roster that follows the server: `status` flips when the fake route is called. */
function server() {
  let status: Member["status"] = "active";
  return {
    members: () => [sofiaFor("tenant_admin", { status })],
    set: (s: Member["status"]) => void (status = s),
    get status() {
      return status;
    },
  };
}

describe("Header actions by power (Part 7)", () => {
  it("suspend_and_revoke_are_tenant_admin_only", async () => {
    mount({ caller: "tenant_admin" });
    expect(await screen.findByRole("button", { name: "Suspend" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Revoke Postgres access now" })).toBeInTheDocument();
  });

  it.each(["project_admin", "developer", "self"] as Caller[])("a_%s_gets_neither_suspend_nor_revoke_nor_reinstate", async (caller) => {
    mount({ caller, members: caller === "self" ? () => [sofiaFor("self")] : undefined });
    await screen.findByRole("heading", { level: 1, name: /Sofia Marchetti/ });
    for (const name of ["Suspend", "Reinstate", "Revoke Postgres access now"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
  });

  it("a_tenant_admin_cannot_suspend_themself_from_their_own_page", async () => {
    mount({ caller: "tenant_admin", memberId: MARA, routes: { [`GET /v1/members/${MARA}/personal-logins`]: ok({ logins: [] }) } });
    await screen.findByRole("heading", { level: 1, name: /Mara Okafor/ });
    expect(screen.queryByRole("button", { name: "Suspend" })).toBeNull();
  });
});

describe("Suspend", () => {
  it("asks_with_the_copy_then_suspends_and_offers_reinstate", async () => {
    const s = server();
    const m = mount({
      caller: "tenant_admin",
      members: s.members,
      logins: () => [LOGIN_LATER],
      routes: { [SUSPEND]: () => (s.set("suspended"), Response.json(sofiaFor("tenant_admin", { status: "suspended" }))) },
    });
    await m.user.click(await screen.findByRole("button", { name: "Suspend" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Suspend Sofia Marchetti?" });
    expect(dialog).toHaveTextContent(/Sofia is signed out of Studio and can't sign in/);
    expect(dialog).toHaveTextContent(/personal Postgres logins stop working at once and every database session/);
    expect(dialog).toHaveTextContent(/project access and roles are kept, so Reinstate gives them back/);
    expect(dialog).toHaveTextContent(/remove (her|him|them|Sofia) from the tenant instead/);
    expect(calls(m.f, "POST", "/v1/members")).toHaveLength(0);
    await m.user.click(within(dialog).getByRole("button", { name: "Suspend Sofia" }));
    await waitFor(() => expect(calls(m.f, "POST", "/v1/members")).toHaveLength(1));
    expect(new URL(calls(m.f, "POST", "/v1/members")[0].url).pathname).toBe(`/v1/members/${SOFIA}/suspend`);
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(await screen.findByRole("button", { name: "Reinstate" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Suspend" })).toBeNull();
    expect((await screen.findAllByText(/^Suspended on /)).length).toBeGreaterThan(0);
  });

  it("cancel_asks_nothing_of_the_server", async () => {
    const m = mount({ caller: "tenant_admin" });
    await m.user.click(await screen.findByRole("button", { name: "Suspend" }));
    await m.user.click(within(await screen.findByRole("alertdialog", { name: "Suspend Sofia Marchetti?" })).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls(m.f, "POST", "/v1/members")).toHaveLength(0);
  });

  it.each([
    [409, "last_tenant_admin", "Sofia is the last tenant admin"],
    [409, "conflict", "Sofia is not an active member"],
    [503, "unavailable", "Studio sign-in could not be locked; retry"],
  ])("a_%s_%s_stays_in_the_dialog_with_the_reason", async (status, code, message) => {
    const m = mount({ caller: "tenant_admin", routes: { [SUSPEND]: apiError(status, code, message) } });
    await m.user.click(await screen.findByRole("button", { name: "Suspend" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Suspend Sofia Marchetti?" });
    await m.user.click(within(dialog).getByRole("button", { name: "Suspend Sofia" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(message);
    // Still open and retryable (the 503 asks for a retry).
    expect(within(dialog).getByRole("button", { name: "Suspend Sofia" })).toBeEnabled();
    expect(screen.queryByRole("button", { name: "Reinstate" })).toBeNull();
  });
});

describe("A suspended member's page", () => {
  const suspended = () => [sofiaFor("tenant_admin", { status: "suspended" })];

  it("says_so_keeps_the_cards_editable_and_reinstates_after_asking", async () => {
    const s = server();
    s.set("suspended");
    const m = mount({
      caller: "tenant_admin",
      members: s.members,
      routes: { [REINSTATE]: () => (s.set("active"), Response.json(sofiaFor("tenant_admin"))) },
    });
    const header = await screen.findByRole("button", { name: "Reinstate" });
    expect(screen.getByText(/can't sign in to Studio/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Suspend" })).toBeNull();
    // Her project access is kept and stays editable.
    const billing = screen.getByRole("region", { name: "Billing app" });
    await m.user.click(await within(billing).findByRole("checkbox", { name: "billing_reader" }));
    expect(within(billing).getByRole("button", { name: "Save" })).toBeEnabled();
    expect(within(billing).getByText(/Effective once reinstated/)).toBeInTheDocument();
    await m.user.click(header);
    const dialog = await screen.findByRole("alertdialog", { name: "Reinstate Sofia Marchetti?" });
    expect(dialog).toHaveTextContent(/Sofia can sign in again with the access listed on this page/);
    expect(dialog).toHaveTextContent(/Personal logins aren't restored; Sofia creates new ones/);
    await m.user.click(within(dialog).getByRole("button", { name: "Reinstate" }));
    await waitFor(() => expect(calls(m.f, "POST", "/v1/members")).toHaveLength(1));
    expect(new URL(calls(m.f, "POST", "/v1/members")[0].url).pathname).toBe(`/v1/members/${SOFIA}/reinstate`);
    expect(await screen.findByRole("button", { name: "Suspend" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Reinstate" })).toBeNull();
  });

  it("a_failed_unlock_stays_in_the_dialog", async () => {
    const m = mount({ caller: "tenant_admin", members: suspended, routes: { [REINSTATE]: apiError(503, "unavailable", "Unlock failed; retry") } });
    await m.user.click(await screen.findByRole("button", { name: "Reinstate" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Reinstate Sofia Marchetti?" });
    await m.user.click(within(dialog).getByRole("button", { name: "Reinstate" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("Unlock failed; retry");
  });
});

describe("Revoke Postgres access now", () => {
  it("names_the_logins_that_stop_asks_then_revokes_and_reports", async () => {
    let logins = [LOGIN_SOON, LOGIN_LATER, LOGIN_REVOKED];
    const m = mount({
      caller: "tenant_admin",
      logins: () => logins,
      routes: {
        [REVOKE]: () => {
          logins = logins.map((l) => (l.status === "active" ? { ...l, status: "revoked" as const, revoked_at: new Date().toISOString(), revoked_reason: "revoked_now" as const } : l));
          return Response.json({ revoked: 2, rotated_branches: 3 });
        },
      },
    });
    await m.user.click(await screen.findByRole("button", { name: "Revoke Postgres access now" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Revoke Sofia's Postgres access now?" });
    expect(dialog).toHaveTextContent(/For a password that may have leaked/);
    // The logins that stop (the active ones), by name on database; not the one already revoked.
    const items = within(dialog).getAllByRole("listitem").map((li) => li.textContent);
    expect(items.filter((t) => /sofia on billing-prod/.test(t ?? ""))).toHaveLength(1);
    expect(items.filter((t) => /sofia_m on warehouse/.test(t ?? ""))).toHaveLength(1);
    expect(items.filter((t) => /sofia on ops/.test(t ?? ""))).toHaveLength(0);
    expect(dialog).toHaveTextContent(/Postgres session \w+ (has|have) open ends, including .*SQL editor and table editor sessions, which reconnect with new passwords/);
    expect(dialog).toHaveTextContent(/Studio access and project roles don't change/);
    expect(calls(m.f, "POST", "/v1/members")).toHaveLength(0);
    await m.user.click(within(dialog).getByRole("button", { name: "Revoke access now" }));
    await waitFor(() => expect(calls(m.f, "POST", "/v1/members")).toHaveLength(1));
    expect(new URL(calls(m.f, "POST", "/v1/members")[0].url).pathname).toBe(`/v1/members/${SOFIA}/revoke-postgres-access`);
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    // The result is said, and the logins list is read again.
    const status = await screen.findByRole("status");
    expect(status).toHaveTextContent(/Revoked 2 logins/);
    expect(status).toHaveTextContent(/3 branches/);
    await waitFor(() => expect(screen.getAllByText(/Revoked/).length).toBeGreaterThan(1));
    // Studio access is untouched: she is still active, not suspended.
    expect(screen.getByRole("button", { name: "Suspend" })).toBeInTheDocument();
  });

  it("with_no_live_logins_the_dialog_says_the_sessions_still_end", async () => {
    const m = mount({ caller: "tenant_admin", logins: () => [LOGIN_REVOKED] });
    await m.user.click(await screen.findByRole("button", { name: "Revoke Postgres access now" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Revoke Sofia's Postgres access now?" });
    expect(within(dialog).queryAllByRole("listitem").filter((li) => /\bon (billing-prod|warehouse|ops)\b/.test(li.textContent ?? ""))).toHaveLength(0);
    expect(dialog).toHaveTextContent(/no personal logins/i);
    expect(within(dialog).getByRole("button", { name: "Revoke access now" })).toBeEnabled();
  });

  it("a_failure_stays_in_the_dialog", async () => {
    const m = mount({ caller: "tenant_admin", logins: () => [LOGIN_SOON], routes: { [REVOKE]: apiError(500, "internal", "could not rotate") } });
    await m.user.click(await screen.findByRole("button", { name: "Revoke Postgres access now" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Revoke Sofia's Postgres access now?" });
    await m.user.click(within(dialog).getByRole("button", { name: "Revoke access now" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("could not rotate");
  });
});
