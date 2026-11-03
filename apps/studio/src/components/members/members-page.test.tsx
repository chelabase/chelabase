import { useState } from "react";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, onTestFinished, vi } from "vitest";
import type { MeResponse as Me, Member } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { MembersPage, type MembersTab } from "./members-page";

const DAY = 86_400_000;
const inDays = (n: number) => new Date(Date.now() + n * DAY).toISOString();

const adminMe: Me = {
  sub: "u1",
  // Not the member list's address: "(you)" comes from the member id.
  email: "ana.personal@elsewhere.test",
  name: "Ana",
  tenant_id: "t1",
  member: { id: "ana", status: "active", tenant_admin: true },
  instance_admin: true, tenant_admins: [],
  projects: [{ project_id: "p1", platform_role: "admin", roles: "all", databases: [] }],
  user: { sub: "u1" },
  tenant: {
    id: "t1",
    slug: "demo",
    display_name: null,
    projects: [{ id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] }],
  },
};
const memberMe: Me = {
  ...adminMe,
  sub: "u2",
  email: "bo@example.test",
  member: { id: "bo", status: "active", tenant_admin: false },
  instance_admin: false,
  projects: [],
};

function member(id: string, extra: Partial<Member>): Member {
  return {
    id, email: `${id}@example.test`, display_name: id, status: "active", tenant_admin: false, has_access: true,
    invite: null, accepted_at: null, suspended_at: null, suspended_by: null, created_at: "2026-10-01T00:00:00Z", access: { level: "none", projects: [] }, ...extra,
  };
}

const people: Member[] = [
  member("ana", { display_name: "Ana", email: "ana@example.test", tenant_admin: true }),
  member("bo", { display_name: "Bo", email: "bo@example.test", has_access: false }),
  member("cy", { status: "invited", display_name: "", invite: { id: "inv-cy", expires_at: inDays(2) + "" } }),
  member("di", { status: "invited", display_name: "", invite: { id: "inv-di", expires_at: inDays(-1) } }),
  member("ed", { status: "pending", display_name: "Ed", email: "ed@example.test" }),
];

const ok = (body: unknown, status = 200): FakeHandler => () => jsonResponse(body, status);

function setup(opts: { me?: Me; members?: Member[]; tab?: MembersTab; routes?: Record<string, FakeHandler> } = {}) {
  const f = fakeFetch({
    "GET /v1/me": ok(opts.me ?? adminMe),
    "GET /v1/members": ok(opts.members ?? people),
    "GET /v1/instance/settings": ok({ open_signup: false, open_signup_switchable: true }),
    "GET /v1/invites/delivery": ok({ email_available: true, reason: null }),
    "GET /v1/access-events": ok({ events: [], next_before: null }),
    ...opts.routes,
  });
  function Harness() {
    const [tab, setTab] = useState<MembersTab>(opts.tab ?? "people");
    return <MembersPage tab={tab} onTabChange={setTab} />;
  }
  return { ...renderWithQuery(<Harness />, { fetch: f }), f };
}

const call = (f: { calls: Request[] }, method: string, path: string) =>
  f.calls.find((r) => r.method === method && new URL(r.url).pathname === path);

async function row(name: string | RegExp) {
  const table = await screen.findByRole("table", { name: "People" });
  return within(table).getByRole("row", { name });
}

describe("MembersPage", () => {
  it("lists_people_with_status", async () => {
    setup();
    expect(await screen.findByText("4 people")).toBeInTheDocument();
    expect(within(await row(/Ana/)).getByText("Active")).toBeInTheDocument();
    expect(within(await row(/Ana/)).getByText("(you)")).toBeInTheDocument();
    expect(within(await row(/Ana/)).getByText("Tenant admin")).toBeInTheDocument();
    expect(within(await row(/Bo/)).getByText("No access yet")).toBeInTheDocument();
    expect(within(await row(/cy@example/)).getByText("Invited · expires in 2 days")).toBeInTheDocument();
    expect(within(await row(/di@example/)).getByText("Invite expired")).toBeInTheDocument();
    // Pending people aren't in the table; they wait under Requests.
    const table = screen.getByRole("table", { name: "People" });
    expect(within(table).queryByText("Ed")).toBeNull();
  });

  it("invite_form_validates_email_and_grants", async () => {
    const { f } = setup({
      routes: {
        "POST /v1/invites": ok({ outcome: "invited", member: member("new", { status: "invited" }) }, 201),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.type(within(dialog).getByLabelText("Emails"), "not-an-email");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    expect(within(dialog).getByText("Enter an email address.")).toBeInTheDocument();
    expect(call(f, "POST", "/v1/invites")).toBeUndefined();

    await userEvent.clear(within(dialog).getByLabelText("Emails"));
    await userEvent.type(within(dialog).getByLabelText("Emails"), "  New@Example.test ");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    // The only project is used now: no second row can be added.
    expect(within(dialog).getByRole("button", { name: "Add a project" })).toBeDisabled();
    // The browser pass: Admin's explanation was said twice; "Add a project" sat tight against
    // the next field; the delivery choices looked like checkboxes.
    await userEvent.click(within(dialog).getByRole("radio", { name: "Admin" }));
    expect(within(dialog).getAllByText(/including the database owner|includes the database owner/)).toHaveLength(1);
    expect(within(dialog).getByRole("button", { name: "Add a project" }).parentElement!.className).toMatch(/\bpb-[1-9]/);
    for (const radio of within(dialog).getAllByRole("radio", { name: /Email|One-time link/ })) {
      expect(radio.querySelector("span")!.className).toContain("rounded-full");
    }
    await userEvent.click(within(dialog).getByRole("radio", { name: /Viewer/ }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    await waitFor(() => expect(call(f, "POST", "/v1/invites")).toBeDefined());
    expect(await call(f, "POST", "/v1/invites")!.clone().json()).toEqual({
      email: "new@example.test",
      delivery: "email",
      grants: [{ project_id: "p1", platform_role: "viewer", pg_roles: [] }],
    });
    expect(await screen.findByRole("status")).toHaveTextContent(
      "Invited new@example.test. The link in their email works for 72 hours.",
    );
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("invite_link_shown_once", async () => {
    // Like a browser: a closing popup's exit transition (150 ms) finishes before it unmounts.
    const proto = Element.prototype as unknown as { getAnimations?: () => unknown[] };
    const original = proto.getAnimations;
    proto.getAnimations = function (this: Element) {
      return this.hasAttribute("data-ending-style")
        ? [{ finished: new Promise((r) => setTimeout(r, 150)), pending: false, playState: "finished" }]
        : [];
    };
    // Like a browser: focus() reads `preventScroll` (Base UI returns focus after an outside press
    // only where it's supported; jsdom ignores the option).
    const focus = HTMLElement.prototype.focus;
    HTMLElement.prototype.focus = function (this: HTMLElement, options?: FocusOptions) {
      void options?.preventScroll;
      return focus.call(this, options);
    };
    onTestFinished(() => {
      proto.getAnimations = original;
      HTMLElement.prototype.focus = focus;
    });
    const url = "http://studio.test/accept-invite?user=z1&code=c1&t=tok";
    const { f, queryClient } = setup({
      routes: {
        "GET /v1/invites/delivery": ok({ email_available: false, reason: "email_not_configured" }),
        "POST /v1/invites": ok({ outcome: "invited", member: member("new", { status: "invited" }), invite_url: url }, 201),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    expect(await within(dialog).findByText(/Email isn't set up on this instance/)).toBeInTheDocument();
    // Without email, only the link is offered.
    expect(within(dialog).queryByRole("radio", { name: /^Email/ })).toBeNull();
    await userEvent.type(within(dialog).getByLabelText("Emails"), "new@example.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Create link" }));
    const panel = await screen.findByRole("dialog", { name: "Invite link for new@example.test" });
    expect(within(panel).getByDisplayValue(url)).toBeInTheDocument();
    expect(within(panel).getByText(/This link is shown once/)).toBeInTheDocument();
    // Focus moves into the link dialog and stays there once the invite dialog's exit transition
    // has ended (its return of focus to "Invite people" must not land behind the open modal).
    await new Promise((r) => setTimeout(r, 400));
    expect(panel.contains(document.activeElement)).toBe(true);
    expect(await call(f, "POST", "/v1/invites")!.clone().json()).toMatchObject({ delivery: "link" });
    // The link never reaches the query or mutation cache.
    const cached = JSON.stringify([
      queryClient.getQueryCache().getAll().map((q) => q.state.data),
      queryClient.getMutationCache().getAll().map((m) => m.state.data),
    ]);
    expect(cached).not.toContain("accept-invite");

    // Closing before copying asks first (Escape and Done alike).
    await userEvent.keyboard("{Escape}");
    expect(within(panel).getByText("You haven't copied the link. It can't be shown again.")).toBeInTheDocument();
    await userEvent.click(within(panel).getByRole("button", { name: "Go back" }));
    // A click outside asks too (the link would be lost), and leaves it open.
    await userEvent.click(document.body);
    expect(within(panel).getByText("You haven't copied the link. It can't be shown again.")).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Invite link for new@example.test" })).toBeInTheDocument();
    await userEvent.click(within(panel).getByRole("button", { name: "Go back" }));
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    await userEvent.click(within(panel).getByRole("button", { name: "Copy link" }));
    expect(writeText).toHaveBeenCalledWith(url);
    await userEvent.click(within(panel).getByRole("button", { name: "Done" }));
    // Gone once its exit transition has ended; focus is back on "Invite people".
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    await waitFor(() => expect(screen.getByRole("button", { name: "Invite people" })).toHaveFocus());
    expect(screen.queryByDisplayValue(url)).toBeNull();
    expect(screen.getByRole("status")).toHaveTextContent("Created an invite link for new@example.test.");
    // Opening the dialog again starts clean.
    await userEvent.click(screen.getByRole("button", { name: "Invite people" }));
    expect(screen.queryByText(url)).toBeNull();
    expect(screen.queryByDisplayValue(url)).toBeNull();
  });

  it("escape_in_an_empty_role_popup_closes_only_the_popup", async () => {
    setup();
    await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.type(within(dialog).getByLabelText("Emails"), "kept@example.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    // The project has no ready database: the popup shows only the "No roles" status.
    await userEvent.click(within(dialog).getByLabelText("Postgres roles"));
    expect(await screen.findByText(/No roles to grant yet/)).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByText(/No roles to grant yet/)).toBeNull());
    expect(screen.getByRole("dialog", { name: "Invite people" })).toBeInTheDocument();
    expect(within(screen.getByRole("dialog", { name: "Invite people" })).getByLabelText("Emails")).toHaveValue("kept@example.test");
  });

  it("invite_link_refused_says_send_by_email", async () => {
    setup({
      routes: {
        // The code decides, never the wording.
        "POST /v1/invites": ok({ error: { code: "link_not_allowed", message: "anything", email_available: true } }, 409),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.click(await within(dialog).findByRole("radio", { name: /One-time link/ }));
    await userEvent.type(within(dialog).getByLabelText("Emails"), "old@example.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Create link" }));
    expect(await within(dialog).findByText(/old@example.test already has an account or a pending invite elsewhere/)).toBeInTheDocument();
    expect(within(dialog).getByRole("radio", { name: /^Email/ })).toBeChecked();
  });

  it("non_admin_sees_read_only", async () => {
    setup({ me: memberMe });
    expect(await screen.findByText("4 people")).toBeInTheDocument();
    expect(screen.getByText("Tenant admins invite and remove people.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Invite people" })).toBeNull();
    expect(screen.queryByRole("button", { name: /More actions/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Rename" })).toBeNull();
    expect(screen.queryByRole("heading", { name: /Requests/ })).toBeNull();
    expect(screen.queryByRole("tab", { name: "Activity" })).toBeNull();
    expect(screen.queryByRole("tab", { name: "Instance" })).toBeNull();
  });

  it("instance_tab_only_with_capability", async () => {
    const first = setup({ me: { ...adminMe, instance_admin: false } });
    expect(await screen.findByRole("tab", { name: "Activity" })).toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: "Instance" })).toBeNull();
    expect(call(first.f, "GET", "/v1/instance/settings")).toBeUndefined();
    first.unmount();

    const hidden = setup({ routes: { "GET /v1/instance/settings": ok({ open_signup: false, open_signup_switchable: false }) } });
    await waitFor(() => expect(call(hidden.f, "GET", "/v1/instance/settings")).toBeDefined());
    await screen.findByText("4 people");
    expect(screen.queryByRole("tab", { name: "Instance" })).toBeNull();
    hidden.unmount();

    const { f } = setup({
      routes: { "PATCH /v1/instance/settings": ok({ open_signup: true, open_signup_switchable: true }) },
    });
    await userEvent.click(await screen.findByRole("tab", { name: "Instance" }));
    const toggle = await screen.findByRole("switch", { name: "Open sign-up" });
    expect(toggle).not.toBeChecked();
    expect(screen.getByText("Off: only invited people can join")).toBeInTheDocument();
    // The browser pass: the switch floated apart from its state; they share one row now.
    expect(toggle.parentElement).toContainElement(screen.getByText("Off: only invited people can join"));
    expect(toggle.parentElement!.className).toContain("items-center");
    expect(toggle.parentElement!.className).not.toContain("flex-col");
    await userEvent.click(toggle);
    await userEvent.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Turn on" }));
    await waitFor(() => expect(call(f, "PATCH", "/v1/instance/settings")).toBeDefined());
    expect(await call(f, "PATCH", "/v1/instance/settings")!.clone().json()).toEqual({ open_signup: true });
    expect(await screen.findByText("On: new accounts wait for approval")).toBeInTheDocument();
  });

  it("requests_approve_and_decline", async () => {
    const two = [...people, member("fy", { status: "pending", display_name: "Fy", email: "fy@example.test" })];
    const { f } = setup({
      members: two,
      routes: {
        "POST /v1/members/ed/approve": ok(member("ed", { display_name: "Ed", email: "ed@example.test" })),
        "POST /v1/members/fy/decline": () => new Response(null, { status: 204 }),
      },
    });
    const requests = await screen.findByRole("region", { name: /Requests/ });
    expect(within(requests).getByText("Ed")).toBeInTheDocument();
    expect(within(requests).getByText("Fy")).toBeInTheDocument();

    await userEvent.click(within(requests).getByRole("button", { name: "Approve Ed" }));
    const approve = await screen.findByRole("dialog", { name: "Approve Ed" });
    await userEvent.click(within(approve).getByRole("button", { name: "Add a project" }));
    await userEvent.click(within(approve).getByRole("button", { name: "Approve" }));
    await waitFor(() => expect(call(f, "POST", "/v1/members/ed/approve")).toBeDefined());
    expect(await call(f, "POST", "/v1/members/ed/approve")!.clone().json()).toEqual({
      grants: [{ project_id: "p1", platform_role: "developer", pg_roles: [] }],
    });
    expect(await screen.findByRole("status")).toHaveTextContent("Approved ed@example.test. They can sign in now.");

    await userEvent.click(within(requests).getByRole("button", { name: "Decline Fy" }));
    const decline = await screen.findByRole("alertdialog", { name: "Decline Fy?" });
    await userEvent.click(within(decline).getByRole("button", { name: "Decline" }));
    await waitFor(() => expect(call(f, "POST", "/v1/members/fy/decline")).toBeDefined());
    expect(await screen.findByText("Declined fy@example.test.")).toBeInTheDocument();
  });

  // The browser pass: handling a request removes its row (and the last one, the whole section),
  // so focus fell to the page body. It moves to the next request, else to "Invite people".
  it("after a request is handled focus moves to the next one, or to Invite people", async () => {
    let list = [...people, member("fy", { status: "pending", display_name: "Fy", email: "fy@example.test" })];
    const settle = (id: string) => {
      list = list.map((m) => (m.id === id ? { ...m, status: id === "ed" ? ("active" as const) : ("removed" as const) } : m));
    };
    setup({
      routes: {
        "GET /v1/members": () => jsonResponse(list),
        "POST /v1/members/ed/decline": () => {
          settle("ed");
          return new Response(null, { status: 204 });
        },
        "POST /v1/members/fy/decline": () => {
          settle("fy");
          return new Response(null, { status: 204 });
        },
      },
    });
    const requests = await screen.findByRole("region", { name: /Requests/ });
    await userEvent.click(within(requests).getByRole("button", { name: "Decline Ed" }));
    await userEvent.click(within(await screen.findByRole("alertdialog", { name: "Decline Ed?" })).getByRole("button", { name: "Decline" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Decline Ed" })).toBeNull());
    await waitFor(() => expect(screen.getByRole("button", { name: "Approve Fy" })).toHaveFocus());
    await userEvent.click(screen.getByRole("button", { name: "Decline Fy" }));
    await userEvent.click(within(await screen.findByRole("alertdialog", { name: "Decline Fy?" })).getByRole("button", { name: "Decline" }));
    await waitFor(() => expect(screen.queryByRole("region", { name: /Requests/ })).toBeNull());
    await waitFor(() => expect(screen.getByRole("button", { name: "Invite people" })).toHaveFocus());
  });

  it("tenant_rename", async () => {
    const { f } = setup({ routes: { "PATCH /v1/tenant": ok({ display_name: "Acme" }) } });
    expect(await screen.findByText("People in demo and what they can reach.")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Rename" }));
    const field = screen.getByLabelText("Tenant name");
    await userEvent.type(field, "  Acme ");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(call(f, "PATCH", "/v1/tenant")).toBeDefined());
    expect(await call(f, "PATCH", "/v1/tenant")!.clone().json()).toEqual({ display_name: "Acme" });
    expect(await screen.findByRole("status")).toHaveTextContent("Renamed the tenant to Acme.");
    // The form goes away: focus comes back to Rename, not to the page body.
    await waitFor(() => expect(screen.getByRole("button", { name: "Rename" })).toHaveFocus());
    // Cancel hands focus back the same way.
    await userEvent.click(screen.getByRole("button", { name: "Rename" }));
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Rename" })).toHaveFocus());
    // Escape in the field cancels too.
    await userEvent.click(screen.getByRole("button", { name: "Rename" }));
    await userEvent.type(screen.getByLabelText("Tenant name"), "Nope");
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByLabelText("Tenant name")).toBeNull();
    await waitFor(() => expect(screen.getByRole("button", { name: "Rename" })).toHaveFocus());
    expect(call(f, "PATCH", "/v1/tenant")).toBeDefined();
    expect(f.calls.filter((r) => r.method === "PATCH").length).toBe(1);
  });

  // The browser pass: the rename's line stayed while later actions added their own.
  it("one status line: a newer action replaces the older line, and changing tab clears it", async () => {
    setup({
      routes: {
        "PATCH /v1/tenant": ok({ display_name: "Acme" }),
        "PATCH /v1/members/bo": ok(member("bo", { tenant_admin: true })),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Rename" }));
    await userEvent.type(screen.getByLabelText("Tenant name"), "Acme");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("status")).toHaveTextContent("Renamed the tenant to Acme.");
    await userEvent.click(screen.getByRole("button", { name: "More actions for Bo" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Make tenant admin" }));
    await userEvent.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Make tenant admin" }));
    await waitFor(() => expect(screen.getAllByRole("status").map((s) => s.textContent)).toEqual(["Bo is now a tenant admin."]));
    await userEvent.click(screen.getByRole("tab", { name: "Activity" }));
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("row menu: make tenant admin confirms, then patches", async () => {
    const { f } = setup({ routes: { "PATCH /v1/members/bo": ok(member("bo", { tenant_admin: true })) } });
    await userEvent.click(await screen.findByRole("button", { name: "More actions for Bo" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Make tenant admin" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Make Bo a tenant admin?" });
    await userEvent.click(within(confirm).getByRole("button", { name: "Make tenant admin" }));
    await waitFor(() => expect(call(f, "PATCH", "/v1/members/bo")).toBeDefined());
    expect(await call(f, "PATCH", "/v1/members/bo")!.clone().json()).toEqual({ tenant_admin: true });
  });

  it("own row: never remove, and the only admin can't step down", async () => {
    setup();
    await userEvent.click(await screen.findByRole("button", { name: "More actions for Ana" }));
    expect(await screen.findByText("You're the only tenant admin.")).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: /Remove/ })).toBeNull();
    expect(screen.queryByRole("menuitem", { name: /Step down/ })).toBeNull();
  });

  it("activity lists events as sentences", async () => {
    setup({
      tab: "activity",
      routes: {
        "GET /v1/access-events": ok({
          events: [
            {
              id: 7, action: "member_removed", at: new Date().toISOString(), actor_member_id: "ana", actor_email: "ana@example.test",
              target_member_id: "zz", target_email: "zz@example.test", project_id: null, project_name: null, database_id: null,
              before: null, after: null,
            },
          ],
          next_before: null,
        }),
      },
    });
    expect(await screen.findByText("Ana removed zz@example.test")).toBeInTheDocument();
  });

  it("activity details are labelled fields, and loading the last page keeps focus in the list", async () => {
    const ev = (id: number, extra: Record<string, unknown> = {}) => ({
      id, action: "member_removed", at: new Date().toISOString(), actor_member_id: "ana", actor_email: "ana@example.test",
      target_member_id: "zz", target_email: `zz${id}@example.test`, project_id: null, project_name: null, database_id: null,
      before: null, after: null, ...extra,
    });
    setup({
      tab: "activity",
      routes: {
        "GET /v1/access-events": (r) =>
          new URL(r.url).searchParams.get("before")
            ? jsonResponse({ events: [ev(1)], next_before: null })
            : jsonResponse({
                events: [
                  ev(3, {
                    action: "member_approved", before: { status: "pending" },
                    after: { status: "active", grants: [{ project_id: "p1", platform_role: "developer", pg_roles: ["billing"] }] },
                  }),
                  ev(2),
                ],
                next_before: 2,
              }),
      },
    });
    const sentence = await screen.findByText("Ana approved zz3@example.test's sign-up with Developer on Shop");
    const row = sentence.closest("li")!;
    await userEvent.click(within(row).getByRole("button", { name: /Show details/ }));
    expect(within(row).getByText("Access")).toBeInTheDocument();
    expect(within(row).getByText("Developer on Shop (billing)")).toBeInTheDocument();
    expect(within(row).queryByText(/project_id/)).toBeNull();
    // "Show older" goes away with the last page: focus moves to the first entry it brought.
    await userEvent.click(screen.getByRole("button", { name: "Show older" }));
    await screen.findByText("Ana removed zz1@example.test");
    expect(screen.queryByRole("button", { name: "Show older" })).toBeNull();
    const first = screen.getByText("Ana removed zz1@example.test").closest("li")!;
    await waitFor(() => expect(within(first).getByRole("button", { name: /Show details/ })).toHaveFocus());
  });

  it("a 422 grant refusal shows under the row of its project", async () => {
    const twoProjects: Me = {
      ...adminMe,
      tenant: {
        ...adminMe.tenant!,
        projects: [
          { id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] },
          { id: "p2", slug: "blog", name: "Blog", status: "active", databases: [{ database_id: "d2", name: "posts", postgres_database: "posts", status: "ready", host: null, port: null, branch_count: 1 }] },
        ],
      },
    };
    setup({
      me: twoProjects,
      routes: {
        "GET /v1/databases/d2/branches": ok([]),
        "POST /v1/invites": ok(
          { error: { code: "invalid_request", message: "`billing` can be held only with the admin platform role", role: "billing", chain: ["billing", "app_login"], why: "login", database_id: "d2", project_id: "p2" } },
          422,
        ),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.type(within(dialog).getByLabelText("Emails"), "new@example.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    const blog = await within(dialog).findByRole("group", { name: "Blog" });
    // Studio's words, from `extra` (never the server's sentence with its backticks).
    expect(within(blog).getByRole("alert")).toHaveTextContent("Only Admin can hold billing on posts: it reaches app_login, a LOGIN role.");
    expect(within(blog).getByText("billing → app_login")).toBeInTheDocument();
    expect(within(within(dialog).getByRole("group", { name: "Shop" })).queryByRole("alert")).toBeNull();
  });

  // M16: the picker reads each database's main branch. A role that is fine there but not on
  // another branch is refused by the server; the 422 names the branch, database and chain, and the
  // picker shows them right under the roles, not only in the row's message.
  it("a 422 naming a non-main branch shows its database, branch and chain inline at the picked role", async () => {
    const blogMe: Me = {
      ...adminMe,
      tenant: {
        ...adminMe.tenant!,
        projects: [
          { id: "p2", slug: "blog", name: "Blog", status: "active", databases: [{ database_id: "d2", name: "posts", postgres_database: "posts", status: "ready", host: null, port: null, branch_count: 2 }] },
        ],
      },
    };
    const main = { id: "b-main", name: "main", parent_branch_id: null, status: "ready" };
    const role = (name: string) => ({
      name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_no_set: [], member_admin: [],
      blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false,
    });
    setup({
      me: blogMe,
      routes: {
        "GET /v1/databases/d2/branches": ok([main]),
        "GET /v1/databases/d2/branches/b-main/roles": ok([role("reports")]),
        "POST /v1/invites": ok(
          {
            error: {
              code: "invalid_request", message: "`reports` can be held only with the admin platform role", role: "reports",
              chain: ["reports", "svc"], database_id: "d2", project_id: "p2", branch_id: "b-dev", branch: "dev", branch_main: false,
            },
          },
          422,
        ),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.type(within(dialog).getByLabelText("Emails"), "new@example.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    const blog = await within(dialog).findByRole("group", { name: "Blog" });
    await userEvent.click(within(blog).getByLabelText("Postgres roles"));
    await userEvent.click(await screen.findByRole("option", { name: /reports/ }));
    await userEvent.click(within(dialog).getByLabelText("Emails"));
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    const inline = await within(blog).findByTestId("refused-role-detail");
    expect(inline).toHaveTextContent("reports → svc");
    expect(inline).toHaveTextContent("on posts, branch dev");
  });

  it("delivery lookup failure falls back to a link", async () => {
    setup({ routes: { "GET /v1/invites/delivery": ok({ error: { code: "unavailable", message: "down" } }, 503) } });
    await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    expect(await within(dialog).findByRole("radio", { name: /One-time link/ })).toBeChecked();
    expect(within(dialog).queryByRole("radio", { name: /^Email/ })).toBeNull();
    expect(within(dialog).getByRole("button", { name: "Create link" })).toBeInTheDocument();
  });

  it("an unmatched 409 shows the server's message", async () => {
    setup({ routes: { "POST /v1/invites": ok({ error: { code: "conflict", message: "something else entirely" } }, 409) } });
    await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.type(within(dialog).getByLabelText("Emails"), "x@example.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("something else entirely");
  });

  it("each invite 409 code reads its own way", async () => {
    const cases: [object, RegExp][] = [
      [{ code: "signup_pending", message: "x", member_id: "ed" }, /signed up and is waiting for approval/],
    ];
    for (const [error, text] of cases) {
      const r = setup({ routes: { "POST /v1/invites": ok({ error }, 409) } });
      await userEvent.click(await screen.findByRole("button", { name: "Invite people" }));
      const dialog = await screen.findByRole("dialog", { name: "Invite people" });
      await userEvent.type(within(dialog).getByLabelText("Emails"), "x@example.test");
      await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
      expect(await within(dialog).findByText(text)).toBeInTheDocument();
      r.unmount();
    }
  });

  it("resend and a new invite link from the row menu", async () => {
    const url = "http://studio.test/accept-invite?user=z&code=c2&t=t2";
    const { f } = setup({
      routes: {
        "POST /v1/invites/inv-cy/resend": async (req) => {
          const body = await req.clone().text();
          return body.includes("link")
            ? jsonResponse({ id: "inv-cy", expires_at: inDays(3), invite_url: url })
            : jsonResponse({ id: "inv-cy", expires_at: inDays(3) });
        },
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "More actions for cy" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Resend invite" }));
    expect(await screen.findByText("Sent a new invite to cy@example.test. The previous link no longer works.")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "More actions for cy" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "New invite link" }));
    const panel = await screen.findByRole("dialog", { name: "New invite link for cy@example.test" });
    expect(within(panel).getByDisplayValue(url)).toBeInTheDocument();
    const bodies = await Promise.all(f.calls.filter((r) => r.url.endsWith("/resend")).map((r) => r.clone().json()));
    expect(bodies).toEqual([{ delivery: "email" }, { delivery: "link" }]);
  });

  // A metadata step answers `unavailable`, an identity step `identity_unavailable`: both are retryable.
  it.each(["unavailable", "identity_unavailable"])("a removal answering 503 %s stays on screen with Retry", async (code) => {
    let attempts = 0;
    setup({
      routes: {
        "DELETE /v1/members/bo": () =>
          ++attempts === 1 ? jsonResponse({ error: { code, message: "down" } }, 503) : new Response(null, { status: 204 }),
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "More actions for Bo" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Remove from tenant" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Remove Bo?" });
    await userEvent.click(within(confirm).getByRole("button", { name: "Remove Bo" }));
    const notice = await screen.findByText("Bo is removed, but signing them out of the identity provider didn't finish.");
    await userEvent.click(within(notice.closest("[role=alert]") as HTMLElement).getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(attempts).toBe(2));
    await waitFor(() => expect(screen.queryByText(/signing them out of the identity provider didn't finish/)).toBeNull());
  });

  // The browser pass: the removed row (and its menu) is gone, so focus fell to the page body.
  it("after a removal focus moves to the next person's menu, or to Invite people", async () => {
    let list = people;
    setup({
      routes: {
        "GET /v1/members": () => jsonResponse(list),
        "DELETE /v1/members/bo": () => {
          list = list.filter((m) => m.id !== "bo");
          return new Response(null, { status: 204 });
        },
        "DELETE /v1/members/di": () => {
          list = list.filter((m) => m.id !== "di");
          return new Response(null, { status: 204 });
        },
        "DELETE /v1/invites/inv-di": () => {
          list = list.filter((m) => m.id !== "di");
          return new Response(null, { status: 204 });
        },
      },
    });
    await userEvent.click(await screen.findByRole("button", { name: "More actions for Bo" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Remove from tenant" }));
    await userEvent.click(within(await screen.findByRole("alertdialog", { name: "Remove Bo?" })).getByRole("button", { name: "Remove Bo" }));
    await waitFor(() => expect(screen.queryByText("bo@example.test")).toBeNull());
    await waitFor(() => expect(screen.getByRole("button", { name: "More actions for cy" })).toHaveFocus());
    // The last row: no next one, so the previous one's menu.
    await userEvent.click(screen.getByRole("button", { name: "More actions for di" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Revoke invite" }));
    await userEvent.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Revoke invite" }));
    await waitFor(() => expect(screen.queryByText("di@example.test")).toBeNull());
    await waitFor(() => expect(screen.getByRole("button", { name: "More actions for cy" })).toHaveFocus());
  });

  it("removing the last tenant admin's flag says why on the row", async () => {
    const twoAdmins = people.map((m) => (m.id === "bo" ? { ...m, tenant_admin: true } : m));
    setup({
      members: twoAdmins,
      routes: { "PATCH /v1/members/bo": ok({ error: { code: "last_tenant_admin", message: "x" } }, 409) },
    });
    await userEvent.click(await screen.findByRole("button", { name: "More actions for Bo" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Remove tenant admin" }));
    expect(await within(await row(/Bo/)).findByRole("alert")).toHaveTextContent(
      "Bo is the last tenant admin. Make someone else a tenant admin first.",
    );
  });

  it("writes refresh the activity log", async () => {
    const { f } = setup({ routes: { "PATCH /v1/members/bo": ok(member("bo", { tenant_admin: true })) } });
    await userEvent.click(await screen.findByRole("tab", { name: "Activity" }));
    await screen.findByText(/No access changes yet/);
    const before = f.calls.filter((r) => r.url.includes("/v1/access-events")).length;
    await userEvent.click(screen.getByRole("tab", { name: "People" }));
    await userEvent.click(await screen.findByRole("button", { name: "More actions for Bo" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Make tenant admin" }));
    await userEvent.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Make tenant admin" }));
    await waitFor(() => expect(call(f, "PATCH", "/v1/members/bo")).toBeDefined());
    await userEvent.click(screen.getByRole("tab", { name: "Activity" }));
    await waitFor(() => expect(f.calls.filter((r) => r.url.includes("/v1/access-events")).length).toBeGreaterThan(before));
  });

  it("turning open sign-up on asks first", async () => {
    const { f } = setup({ routes: { "PATCH /v1/instance/settings": ok({ open_signup: true, open_signup_switchable: true }) } });
    await userEvent.click(await screen.findByRole("tab", { name: "Instance" }));
    await userEvent.click(await screen.findByRole("switch", { name: "Open sign-up" }));
    const confirm = await screen.findByRole("alertdialog", { name: "Turn on open sign-up?" });
    expect(within(confirm).getByText(/anyone who reaches the sign-in page can request access/i)).toBeInTheDocument();
    expect(call(f, "PATCH", "/v1/instance/settings")).toBeUndefined();
    await userEvent.click(within(confirm).getByRole("button", { name: "Turn on" }));
    await waitFor(() => expect(call(f, "PATCH", "/v1/instance/settings")).toBeDefined());
  });
});
