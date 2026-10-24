import { act, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { AccessEvent } from "@chelabase/management-client";
import { jsonResponse } from "@/test/fake-fetch";
import { ANA, MARA, P1, P2, P3, SELF_GRANTS, SOFIA, STRANGER, calls, mount, ok, sofiaFor } from "./test-setup";
import { person } from "../hub/fixtures";

const card = (name: string) => screen.findByRole("region", { name });
const pathsOf = (f: { calls: Request[] }) => f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`);

describe("MemberPage: the page per caller", () => {
  it("shows_the_header_for_a_tenant_admin", async () => {
    mount({ caller: "tenant_admin" });
    expect(await screen.findByRole("heading", { level: 1, name: /Sofia Marchetti/ })).toBeInTheDocument();
    expect(screen.getByText("sofia@northwind.dev")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Members/ })).toBeInTheDocument();
  });

  it("a_tenant_admin_gets_one_card_per_project_with_its_own_save", async () => {
    const { f, user } = mount({
      caller: "tenant_admin",
      routes: { [`PUT /v1/projects/${P1}/grants/${SOFIA}`]: ok({ ...SELF_GRANTS[P1], pg_roles: ["billing_editor", "billing_reader"] }) },
    });
    const billing = await card("Billing app");
    const warehouse = await card("Data warehouse");
    // Cards exist for projects Sofia holds, not for the one she doesn't.
    expect(screen.queryByRole("region", { name: "Ops console" })).toBeNull();
    expect(within(billing).queryByRole("button", { name: "Save" })).toBeNull();
    expect(within(warehouse).queryByRole("button", { name: "Save" })).toBeNull();

    await user.click(await within(billing).findByRole("checkbox", { name: "billing_reader" }));
    expect(within(billing).getByRole("button", { name: "Save" })).toBeEnabled();
    // One Save per card (R2): the other card is untouched.
    expect(within(warehouse).queryByRole("button", { name: "Save" })).toBeNull();
    await user.click(within(billing).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/projects")).toHaveLength(1));
    const put = calls(f, "PUT", "/v1/projects")[0];
    expect(new URL(put.url).pathname).toBe(`/v1/projects/${P1}/grants/${SOFIA}`);
    // The own grant only (MH-38a).
    const body = await put.clone().json();
    expect(Object.keys(body).sort()).toEqual(["pg_roles", "platform_role"]);
    expect(body.platform_role).toBe("developer");
    expect([...body.pg_roles].sort()).toEqual(["billing_editor", "billing_reader"]);
  });

  it("a_tenant_admin_target_gets_read_only_cards", async () => {
    mount({
      caller: "tenant_admin",
      memberId: ANA,
      members: () => [
        person(ANA, { name: "Ana Ng", level: "tenant_admin", projects: [[P1, "admin"], [P2, "admin"], [P3, "admin"]] }),
        person(MARA, { name: "Mara Okafor", level: "tenant_admin" }),
      ],
      routes: { [`GET /v1/members/${ANA}/personal-logins`]: ok({ logins: [] }) },
    });
    const billing = await card("Billing app");
    expect(within(billing).getByText("Admin on every project, as tenant admin")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    expect(screen.queryAllByRole("checkbox")).toHaveLength(0);
    expect(screen.queryAllByRole("radio")).toHaveLength(0);
    // Nothing to add: a tenant admin is on every project.
    expect(screen.queryByRole("heading", { name: "Add access to a project" })).toBeNull();
  });

  it("a_project_admin_sees_and_edits_only_their_projects", async () => {
    const { f, user } = mount({
      caller: "project_admin",
      routes: { [`PUT /v1/projects/${P1}/grants/${SOFIA}`]: ok(SELF_GRANTS[P1]) },
    });
    const billing = await card("Billing app");
    expect(screen.queryByRole("region", { name: "Data warehouse" })).toBeNull();
    await user.click(await within(billing).findByRole("checkbox", { name: "billing_reader" }));
    await user.click(within(billing).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/projects")).toHaveLength(1));
    // Never a call about a project the caller doesn't administer.
    expect(pathsOf(f).filter((p) => p.includes(P2) || p.includes(P3))).toEqual([]);
    // Not tenant-admin powers.
    for (const name of ["Suspend", "Revoke Postgres access now", "Make tenant admin"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    // Nothing to add: the only project the caller administers is on the page.
    expect(screen.queryByRole("heading", { name: "Add access to a project" })).toBeNull();
  });

  it("a_developer_sees_platform_roles_per_visible_project_and_nothing_else", async () => {
    const { f } = mount({ caller: "developer" });
    const billing = await card("Billing app");
    expect(within(billing).getByText("Developer")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    expect(screen.queryAllByRole("radio")).toHaveLength(0);
    expect(screen.queryAllByRole("checkbox")).toHaveLength(0);
    // Never another member's Postgres roles or overrides (MH-35): the grants list isn't even asked for.
    expect(pathsOf(f).filter((p) => p.endsWith("/grants"))).toEqual([]);
    expect(screen.queryByText("billing_editor")).toBeNull();
    expect(screen.queryByRole("button", { name: "New login" })).toBeNull();
    expect(screen.queryByRole("heading", { name: "Add access to a project" })).toBeNull();
  });

  it("own_page_is_read_only_except_personal_logins", async () => {
    const { f } = mount({ caller: "self", memberId: SOFIA, members: () => [sofiaFor("self")] });
    await card("Billing app");
    expect(screen.getByText("Ask a tenant admin or a project Admin to change your access.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save" })).toBeNull();
    for (const el of [...screen.queryAllByRole("radio"), ...screen.queryAllByRole("checkbox")]) expect(el).toBeDisabled();
    for (const name of ["Suspend", "Revoke Postgres access now", "Remove from tenant"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    expect(screen.queryByRole("heading", { name: "Add access to a project" })).toBeNull();
    // Her logins stay editable.
    expect(await screen.findByRole("button", { name: "New login" })).toBeEnabled();
    expect(calls(f, "PUT", "/v1/projects")).toHaveLength(0);
  });
});

describe("MemberPage: Add access to a project", () => {
  it("adds_a_card_for_a_project_the_member_has_no_grant_on_and_saves_it", async () => {
    const { f, user } = mount({
      caller: "tenant_admin",
      routes: { [`PUT /v1/projects/${P3}/grants/${SOFIA}`]: ok({ member_id: SOFIA, platform_role: "developer", pg_roles: [], overrides: [] }) },
    });
    await card("Billing app");
    expect(await screen.findByRole("heading", { name: "Add access to a project" })).toBeInTheDocument();
    const pick = screen.getByLabelText("Project to add");
    // Only projects the member is not on yet.
    expect(within(pick).getAllByRole("option").map((o) => o.textContent)).toEqual(expect.arrayContaining(["Ops console"]));
    expect(within(pick).queryByRole("option", { name: "Billing app" })).toBeNull();
    await user.selectOptions(pick, P3);
    await user.click(screen.getByRole("button", { name: "Add" }));
    const ops = await card("Ops console");
    await user.click(within(ops).getByRole("radio", { name: /^Developer/ }));
    await user.click(within(ops).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/projects")).toHaveLength(1));
    const put = calls(f, "PUT", "/v1/projects")[0];
    expect(new URL(put.url).pathname).toBe(`/v1/projects/${P3}/grants/${SOFIA}`);
    expect(await put.clone().json()).toEqual({ platform_role: "developer", pg_roles: [] });
  });
});

describe("MemberPage: leaving with unsaved cards", () => {
  async function dirty() {
    const m = mount({ caller: "tenant_admin" });
    const billing = await card("Billing app");
    await m.user.click(await within(billing).findByRole("checkbox", { name: "billing_reader" }));
    return m;
  }

  it("leaving_with_unsaved_cards_asks_first_and_keep_editing_stays", async () => {
    const { router, user } = await dirty();
    act(() => void router.navigate({ href: "/other" }));
    expect(await screen.findByText("Discard unsaved changes?")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    await waitFor(() => expect(screen.queryByText("Discard unsaved changes?")).toBeNull());
    expect(router.state.location.pathname).toBe(`/app/members/${SOFIA}`);
    // The edit is still there.
    expect(within(screen.getByRole("region", { name: "Billing app" })).getByRole("checkbox", { name: "billing_reader" })).toBeChecked();
  });

  it("leaving_with_unsaved_cards_then_discard_navigates", async () => {
    const { router, user } = await dirty();
    act(() => void router.navigate({ href: "/other" }));
    await screen.findByText("Discard unsaved changes?");
    await user.click(screen.getByRole("button", { name: "Discard" }));
    expect(await screen.findByText("elsewhere")).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/other");
  });

  it("a_clean_page_leaves_without_asking", async () => {
    const { router } = mount({ caller: "tenant_admin" });
    await card("Billing app");
    await act(async () => void (await router.navigate({ href: "/other" })));
    expect(await screen.findByText("elsewhere")).toBeInTheDocument();
    expect(screen.queryByText("Discard unsaved changes?")).toBeNull();
  });

  it("a_saved_card_no_longer_asks", async () => {
    const { router, user, f } = mount({
      caller: "tenant_admin",
      routes: { [`PUT /v1/projects/${P1}/grants/${SOFIA}`]: ok({ ...SELF_GRANTS[P1], pg_roles: ["billing_editor", "billing_reader"] }) },
    });
    const billing = await card("Billing app");
    await user.click(await within(billing).findByRole("checkbox", { name: "billing_reader" }));
    await user.click(within(billing).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(calls(f, "PUT", "/v1/projects")).toHaveLength(1));
    await waitFor(() => expect(within(screen.getByRole("region", { name: "Billing app" })).queryByRole("button", { name: "Save" })).toBeNull());
    await act(async () => void (await router.navigate({ href: "/other" })));
    expect(await screen.findByText("elsewhere")).toBeInTheDocument();
  });
});

describe("MemberPage: not found", () => {
  it("an_unknown_member_id_is_the_not_found_state", async () => {
    mount({ caller: "tenant_admin", memberId: STRANGER });
    expect(await screen.findByRole("heading", { name: "Member not found" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Suspend" })).toBeNull();
  });

  it("a_non_uuid_member_id_is_not_found_and_asks_for_nothing_about_it", async () => {
    const { f } = mount({ caller: "tenant_admin", memberId: "not-a-uuid" });
    expect(await screen.findByRole("heading", { name: "Member not found" })).toBeInTheDocument();
    expect(pathsOf(f).filter((p) => p.includes("not-a-uuid"))).toEqual([]);
  });
});

describe("MemberPage: Activity", () => {
  const event: AccessEvent = {
    id: 7, action: "admin_set", at: "2026-10-20T10:00:00Z", actor_member_id: MARA, actor_email: "mara@northwind.dev",
    target_member_id: SOFIA, target_email: "sofia@northwind.dev", project_id: null, project_name: null, database_id: null, branch_id: null,
    before: null, after: null,
  };

  it("a_tenant_admin_reads_the_member_filtered_tenant_log", async () => {
    const { f } = mount({ caller: "tenant_admin", events: [event] });
    expect(await screen.findByText("Mara Okafor made Sofia Marchetti a tenant admin")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Activity" })).toBeInTheDocument();
    const req = calls(f, "GET", "/v1/access-events")[0];
    expect(new URL(req.url).searchParams.get("member_id")).toBe(SOFIA);
  });

  it("a_project_admin_reads_the_member_filtered_log_of_each_visible_project_only", async () => {
    const { f } = mount({ caller: "project_admin", events: [event] });
    expect(await screen.findByText("Mara Okafor made Sofia Marchetti a tenant admin")).toBeInTheDocument();
    const mine = calls(f, "GET", `/v1/projects/${P1}/access-events`);
    expect(mine.length).toBeGreaterThan(0);
    expect(new URL(mine[0].url).searchParams.get("member_id")).toBe(SOFIA);
    expect(calls(f, "GET", "/v1/access-events")).toHaveLength(0);
    expect(pathsOf(f).filter((p) => p.includes(`${P2}/access-events`) || p.includes(`${P3}/access-events`))).toEqual([]);
  });

  it.each(["developer", "self"] as const)("a_%s_sees_no_activity_and_asks_for_none", async (caller) => {
    const { f } = mount({ caller, events: [event] });
    await card("Billing app");
    expect(screen.queryByRole("heading", { name: "Activity" })).toBeNull();
    expect(pathsOf(f).filter((p) => p.includes("access-events"))).toEqual([]);
  });

  it("shows_an_empty_log_in_words", async () => {
    mount({ caller: "tenant_admin", events: [] });
    expect(await screen.findByRole("heading", { name: "Activity" })).toBeInTheDocument();
    expect(await screen.findByText(/No access changes/)).toBeInTheDocument();
  });
});

describe("MemberPage: failures to load", () => {
  it("shows_the_error_and_a_retry_when_the_roster_fails", async () => {
    mount({ caller: "tenant_admin", routes: { "GET /v1/members": () => jsonResponse({ error: { code: "internal", message: "boom" } }, 500) } });
    expect(await screen.findByRole("alert")).toHaveTextContent(/boom/);
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });
});
