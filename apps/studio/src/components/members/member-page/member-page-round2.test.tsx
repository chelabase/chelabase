import { screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Member, ProjectGrant } from "@chelabase/management-client";
import { jsonResponse } from "@/test/fake-fetch";
import { copyFor } from "../people-tab";
import { D1, P1, SELF_GRANTS, SOFIA, apiError, calls, mount, ok, sofiaFor, type Caller } from "./test-setup";

/** Round 2 of S2: the spec's Part 3 items round 1 left out. */

// --- Narrow on a database ---------------------------------------------------------------------

const OVERRIDES = `/v1/projects/${P1}/grants/${SOFIA}/overrides`;
const GRANTS_P1 = `GET /v1/projects/${P1}/grants`;

/** A server that remembers the override the page writes, so the refetched grant carries it. */
function narrowable() {
  let overrides: { database_id: string; platform_role: string | null; pg_roles: string[] | null }[] = [];
  const grant = (): ProjectGrant => ({ ...SELF_GRANTS[P1], overrides } as ProjectGrant);
  return {
    routes: {
      [GRANTS_P1]: () => jsonResponse([grant()]),
      [`PUT ${OVERRIDES}`]: async (req: Request) => {
        const body = await req.clone().json();
        overrides = [body];
        return jsonResponse(body);
      },
      [`DELETE ${OVERRIDES}`]: () => {
        overrides = [];
        return new Response(null, { status: 204 });
      },
    },
  };
}

const billing = () => screen.findByRole("region", { name: "Billing app" });
/** The collapsed section's toggle inside a card. */
const toggle = (card: HTMLElement) => within(card).findByRole("button", { name: "Narrow on a database" });

describe("Narrow on a database (Part 3, section 1)", () => {
  it("is_collapsed_in_each_card_and_opens_to_todays_per_database_lines", async () => {
    const m = mount({ caller: "tenant_admin", routes: narrowable().routes });
    const card = await billing();
    const t = await toggle(card);
    expect(t).toHaveAttribute("aria-expanded", "false");
    // Nothing of the editor is on screen while collapsed.
    expect(within(card).queryByRole("button", { name: "Narrow access on billing-prod" })).toBeNull();
    await m.user.click(t);
    expect(t).toHaveAttribute("aria-expanded", "true");
    const line = within(card).getByRole("group", { name: "billing-prod" });
    expect(within(line).getByText("Same as the project")).toBeInTheDocument();
    expect(within(line).getByRole("button", { name: "Narrow access on billing-prod" })).toBeInTheDocument();
    // One toggle per card: the warehouse card has its own, still collapsed.
    const warehouse = await screen.findByRole("region", { name: "Data warehouse" });
    expect(await toggle(warehouse)).toHaveAttribute("aria-expanded", "false");
  });

  it("narrows_with_only_narrower_choices_and_puts_the_override_to_the_api", async () => {
    const m = mount({ caller: "tenant_admin", routes: narrowable().routes });
    const card = await billing();
    await m.user.click(await toggle(card));
    await m.user.click(within(card).getByRole("button", { name: "Narrow access on billing-prod" }));
    const form = await within(card).findByRole("form", { name: "Narrow on billing-prod" });
    const save = within(form).getByRole("button", { name: "Save narrowing" });
    // Nothing is narrower yet.
    expect(save).toBeDisabled();
    await m.user.click(within(form).getByRole("checkbox", { name: "Same as the project" }));
    // The grant is Developer: Admin is not offered at all.
    expect(within(form).getByRole("radio", { name: "Viewer" })).toBeInTheDocument();
    expect(within(form).queryByRole("radio", { name: "Admin" })).toBeNull();
    // The card's roles (billing_editor) are the only ones offered, never the branch's other roles.
    await m.user.click(within(form).getByRole("checkbox", { name: "All of the project's roles" }));
    expect(within(form).getByRole("checkbox", { name: "billing_editor" })).toBeInTheDocument();
    expect(within(form).queryByRole("checkbox", { name: "billing_reader" })).toBeNull();
    await m.user.click(within(form).getByRole("checkbox", { name: "billing_editor" }));
    await m.user.click(within(form).getByRole("radio", { name: "Viewer" }));
    expect(save).toBeEnabled();
    await m.user.click(save);
    await waitFor(() => expect(calls(m.f, "PUT", OVERRIDES)).toHaveLength(1));
    expect(await calls(m.f, "PUT", OVERRIDES)[0].clone().json()).toEqual({ database_id: D1, platform_role: "viewer", pg_roles: [] });
    expect(await screen.findByRole("status")).toHaveTextContent("Narrowed Sofia Marchetti on billing-prod.");
  });

  it("the_effective_line_reflects_the_narrowing_and_stop_narrowing_removes_it", async () => {
    const m = mount({ caller: "tenant_admin", routes: narrowable().routes });
    const card = await billing();
    await within(card).findByText(/^Effective/);
    const effective = () => within(card).getByText(/^Effective/).closest("p")!;
    expect(effective()).not.toHaveTextContent(/billing-prod/);
    await m.user.click(await toggle(card));
    await m.user.click(within(card).getByRole("button", { name: "Narrow access on billing-prod" }));
    const form = await within(card).findByRole("form", { name: "Narrow on billing-prod" });
    await m.user.click(within(form).getByRole("checkbox", { name: "Same as the project" }));
    await m.user.click(within(form).getByRole("radio", { name: "Viewer" }));
    await m.user.click(within(form).getByRole("button", { name: "Save narrowing" }));
    // The Effective line names the database and the lower role it is narrowed to.
    await waitFor(() => expect(effective()).toHaveTextContent(/billing-prod.*Viewer/));
    // The unchanged project role is still said.
    expect(effective()).toHaveTextContent(/Developer/);
    await m.user.click(within(card).getByRole("button", { name: "Stop narrowing on billing-prod" }));
    await waitFor(() => expect(calls(m.f, "DELETE", OVERRIDES)).toHaveLength(1));
    expect(new URL(calls(m.f, "DELETE", OVERRIDES)[0].url).searchParams.get("database_id")).toBe(D1);
    await waitFor(() => expect(effective()).not.toHaveTextContent(/billing-prod/));
  });

  it("an_existing_override_shows_in_the_card_and_in_its_effective_line", async () => {
    const m = mount({
      caller: "tenant_admin",
      grants: { ...SELF_GRANTS, [P1]: { ...SELF_GRANTS[P1], overrides: [{ database_id: D1, platform_role: "viewer", pg_roles: null }] } as ProjectGrant },
    });
    const card = await billing();
    expect((await within(card).findByText(/^Effective/)).closest("p")).toHaveTextContent(/billing-prod.*Viewer/);
    await m.user.click(await toggle(card));
    const line = within(card).getByRole("group", { name: "billing-prod" });
    expect(within(line).getByText("Viewer")).toBeInTheDocument();
    expect(within(line).getByRole("button", { name: "Stop narrowing on billing-prod" })).toBeInTheDocument();
  });

  it("a_refusal_stays_in_the_card_with_the_reason", async () => {
    const m = mount({
      caller: "tenant_admin",
      routes: { ...narrowable().routes, [`PUT ${OVERRIDES}`]: apiError(422, "invalid_request", "That narrows nothing.") },
    });
    const card = await billing();
    await m.user.click(await toggle(card));
    await m.user.click(within(card).getByRole("button", { name: "Narrow access on billing-prod" }));
    const form = await within(card).findByRole("form", { name: "Narrow on billing-prod" });
    await m.user.click(within(form).getByRole("checkbox", { name: "Same as the project" }));
    await m.user.click(within(form).getByRole("radio", { name: "Viewer" }));
    await m.user.click(within(form).getByRole("button", { name: "Save narrowing" }));
    expect(await within(card).findByText("That narrows nothing.")).toBeInTheDocument();
  });

  it("a_project_admin_narrows_on_their_project", async () => {
    mount({ caller: "project_admin" });
    const card = await billing();
    expect(await toggle(card)).toBeInTheDocument();
  });

  it.each(["developer", "self"] as Caller[])("a_%s_never_sees_the_narrowing_section", async (caller) => {
    mount({ caller, members: caller === "self" ? () => [sofiaFor("self")] : undefined, memberId: caller === "self" ? SOFIA : undefined });
    await billing();
    expect(screen.queryByRole("button", { name: "Narrow on a database" })).toBeNull();
    expect(screen.queryByRole("button", { name: /Narrow access on/ })).toBeNull();
  });

  it("a_tenant_admin_target_has_nothing_to_narrow", async () => {
    // Read-only cards ("Admin on every project, as tenant admin"): no section, no override call.
    const m = mount({ caller: "tenant_admin", members: () => [sofiaFor("tenant_admin", { tenant_admin: true })] });
    const card = await billing();
    expect(within(card).getByText("Admin on every project, as tenant admin")).toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: "Narrow on a database" })).toBeNull();
    expect(calls(m.f, "PUT", "/v1/projects")).toHaveLength(0);
  });
});

// --- Invite actions on an invited member's page -----------------------------------------------

const INVITE = "inv-sofia";
const EMAIL = "sofia@northwind.dev";
const URL_ONCE = "http://studio.test/accept-invite?user=s&code=c9&t=t9";
const invited = (): Member =>
  sofiaFor("tenant_admin", {
    status: "invited",
    display_name: "",
    has_access: false,
    invite: { id: INVITE, expires_at: new Date(Date.now() + 2 * 86_400_000).toISOString() },
    access: { level: "none", projects: [] },
  } as Partial<Member>);

const DELIVERY = "GET /v1/invites/delivery";
const RESEND = `POST /v1/invites/${INVITE}/resend`;
const mountInvited = (routes: Record<string, ReturnType<typeof ok>> = {}, caller: Caller = "tenant_admin") =>
  mount({ caller, members: () => [invited()], routes: { [DELIVERY]: ok({ email_available: true, reason: null }), ...routes } });

describe("Invite actions on an invited member's page (Part 3 header)", () => {
  it("offers_all_three_when_email_delivery_is_available", async () => {
    mountInvited();
    expect(await screen.findByRole("button", { name: "Resend invite" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New invite link" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Revoke invite" })).toBeInTheDocument();
  });

  it("without_email_delivery_there_is_no_resend_but_the_link_and_revoke_remain", async () => {
    mountInvited({ [DELIVERY]: ok({ email_available: false, reason: "no_email_provider" }) });
    expect(await screen.findByRole("button", { name: "New invite link" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Revoke invite" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Resend invite" })).toBeNull();
  });

  it("a_failed_delivery_lookup_hides_resend_too", async () => {
    mountInvited({ [DELIVERY]: apiError(503, "unavailable", "down") });
    expect(await screen.findByRole("button", { name: "New invite link" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Resend invite" })).toBeNull();
  });

  it("resend_sends_by_email_and_says_so", async () => {
    const m = mountInvited({ [RESEND]: ok({ id: INVITE, expires_at: new Date(Date.now() + 3 * 86_400_000).toISOString() }) });
    await m.user.click(await screen.findByRole("button", { name: "Resend invite" }));
    expect(await screen.findByText(`Sent a new invite to ${EMAIL}. The previous link no longer works.`)).toBeInTheDocument();
    expect(calls(m.f, "POST", `/v1/invites/${INVITE}/resend`)).toHaveLength(1);
    expect(await calls(m.f, "POST", `/v1/invites/${INVITE}/resend`)[0].clone().json()).toEqual({ delivery: "email" });
  });

  it("new_invite_link_shows_the_link_once_in_the_roster_dialog", async () => {
    const m = mountInvited({ [RESEND]: ok({ id: INVITE, expires_at: new Date(Date.now() + 3 * 86_400_000).toISOString(), invite_url: URL_ONCE }) });
    await m.user.click(await screen.findByRole("button", { name: "New invite link" }));
    const dialog = await screen.findByRole("dialog", { name: `New invite link for ${EMAIL}` });
    expect(within(dialog).getByDisplayValue(URL_ONCE)).toBeInTheDocument();
    expect(within(dialog).getByText(/This link is shown once/)).toBeInTheDocument();
    expect(await calls(m.f, "POST", `/v1/invites/${INVITE}/resend`)[0].clone().json()).toEqual({ delivery: "link" });
    // The URL never reaches a cache (it is shown once).
    const cached = JSON.stringify(m.queryClient.getQueryCache().getAll().map((q) => q.state.data));
    expect(cached).not.toContain(URL_ONCE);
  });

  it("a_closed_invite_says_why", async () => {
    const m = mountInvited({ [RESEND]: apiError(409, "invite_closed", "closed") });
    await m.user.click(await screen.findByRole("button", { name: "New invite link" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("This invite was revoked or accepted in the meantime.");
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(calls(m.f, "POST", "/v1/invites")).toHaveLength(1);
  });

  it("revoke_asks_with_the_rosters_copy_then_deletes_the_invite_and_leaves_the_page", async () => {
    const m = mountInvited({ [`DELETE /v1/invites/${INVITE}`]: () => new Response(null, { status: 204 }) });
    await m.user.click(await screen.findByRole("button", { name: "Revoke invite" }));
    const dialog = await screen.findByRole("alertdialog", { name: `Revoke the invite to ${EMAIL}?` });
    const copy = copyFor({ kind: "revoke", member: invited() } as Parameters<typeof copyFor>[0]);
    expect(dialog).toHaveTextContent(copy.body);
    expect(calls(m.f, "DELETE", "/v1/invites")).toHaveLength(0);
    await m.user.click(within(dialog).getByRole("button", { name: copy.confirm }));
    await waitFor(() => expect(calls(m.f, "DELETE", "/v1/invites")).toHaveLength(1));
    expect(new URL(calls(m.f, "DELETE", "/v1/invites")[0].url).pathname).toBe(`/v1/invites/${INVITE}`);
    // The invited member is gone: the page goes back to the roster.
    expect(await screen.findByText("members list route")).toBeInTheDocument();
  });

  it("cancel_revokes_nothing", async () => {
    const m = mountInvited();
    await m.user.click(await screen.findByRole("button", { name: "Revoke invite" }));
    await m.user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls(m.f, "DELETE", "/v1/invites")).toHaveLength(0);
  });

  it.each(["project_admin", "developer"] as Caller[])("a_%s_gets_no_invite_actions", async (caller) => {
    const m = mountInvited({}, caller);
    await screen.findByRole("heading", { level: 1 });
    for (const name of ["Resend invite", "New invite link", "Revoke invite"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    // And the page never asks whether email is available.
    expect(m.f.calls.some((r) => new URL(r.url).pathname === "/v1/invites/delivery")).toBe(false);
  });

  it("an_active_member_has_no_invite_actions", async () => {
    mount({ caller: "tenant_admin", routes: { [DELIVERY]: ok({ email_available: true, reason: null }) } });
    await screen.findByRole("button", { name: "Suspend" });
    for (const name of ["Resend invite", "New invite link", "Revoke invite"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
  });
});
