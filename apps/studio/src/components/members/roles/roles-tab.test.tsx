import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import {
  ANA, B1, B1B, B1DEV, BIDI, BIDI_ESCAPED, D1, D1B, D2, HIDDEN_ROLES, P1, P2, PRI, PG, ROLES_URL, SOFIA, VISIBLE_ROLES,
  apiError, bodies, calls, mountTab, ok, role,
} from "./test-setup";

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const search = (m: ReturnType<typeof mountTab>) => m.router.state.location.search as Record<string, string | undefined>;
const roleLink = (name: string) => screen.findByRole("link", { name: new RegExp(`^${name}\\b`) });
const text = () => document.body.textContent ?? "";

describe("picker: project, database, branch, Postgres database", () => {
  it("defaults_to_the_first_project_database_and_the_main_branch", async () => {
    const m = mountTab();
    await roleLink("billing_editor");
    expect(screen.getByLabelText("Project")).toHaveDisplayValue("Billing app");
    expect(screen.getByLabelText("Database")).toHaveDisplayValue("billing-prod");
    expect(screen.getByLabelText("Branch")).toHaveDisplayValue("main · protected");
    expect(screen.getByLabelText("Postgres database")).toHaveDisplayValue(PG);
    expect(calls(m.f, "GET", ROLES_URL)).toHaveLength(1);
  });

  it("offers_the_callers_projects_and_the_projects_databases", async () => {
    mountTab();
    await roleLink("billing_editor");
    expect(within(screen.getByLabelText("Project")).getAllByRole("option").map((o) => o.textContent)).toEqual(["Billing app", "Data warehouse"]);
    expect(within(screen.getByLabelText("Database")).getAllByRole("option").map((o) => o.textContent)).toEqual(["billing-prod", "billing-staging"]);
    expect(within(screen.getByLabelText("Postgres database")).getAllByRole("option").map((o) => o.textContent)).toEqual([PG, "analytics"]);
  });

  it("holds_the_choice_in_the_search_params_and_reads_it_back", async () => {
    const m = mountTab({}, `/app/members?tab=roles&project=${P2}`);
    await waitFor(() => expect(screen.getByLabelText("Project")).toHaveDisplayValue("Data warehouse"));
    expect(screen.getByLabelText("Database")).toHaveDisplayValue("warehouse");
    await waitFor(() => expect(calls(m.f, "GET", `/v1/databases/${D2}/branches/${B2()}/roles`)).toHaveLength(1));
  });

  it("choosing_a_branch_loads_its_roles_and_writes_branch_to_the_url", async () => {
    const m = mountTab();
    await roleLink("billing_editor");
    await m.user.selectOptions(screen.getByLabelText("Branch"), "dev");
    expect(await roleLink("dev_only")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /billing_editor/ })).toBeNull();
    expect(search(m)).toMatchObject({ tab: "roles", database: D1, branch: B1DEV });
    // dev isn't protected: no banner.
    expect(screen.queryByText(/is protected/)).toBeNull();
  });

  it("choosing_a_database_resets_the_branch_to_its_main_and_drops_the_stale_postgres_database", async () => {
    const m = mountTab({}, `/app/members?tab=roles&database=${D1}&branch=${B1DEV}&pg_database=analytics`);
    await waitFor(() => expect(screen.getByLabelText("Branch")).toHaveDisplayValue("dev"));
    await m.user.selectOptions(screen.getByLabelText("Database"), "billing-staging");
    await waitFor(() => expect(calls(m.f, "GET", `/v1/databases/${D1B}/branches/${B1B}/roles`)).toHaveLength(1));
    expect(screen.getByLabelText("Branch")).toHaveDisplayValue("main");
    expect(search(m).database).toBe(D1B);
    expect(search(m).branch === undefined || search(m).branch === B1B).toBe(true);
    expect(search(m).pg_database).toBeUndefined();
  });

  it("choosing_a_project_goes_to_that_projects_first_database", async () => {
    const m = mountTab();
    await roleLink("billing_editor");
    await m.user.selectOptions(screen.getByLabelText("Project"), "Data warehouse");
    await waitFor(() => expect(screen.getByLabelText("Database")).toHaveDisplayValue("warehouse"));
    expect(search(m).project).toBe(P2);
  });

  it("drops_search_params_that_are_not_valid_or_not_the_callers", async () => {
    // Not a uuid, an unknown view, a branch that isn't the database's, a project the caller can't see.
    const m = mountTab({ caller: "developer" }, `/app/members?tab=roles&project=${P2}&database=nope&branch=${B1B}&view=bogus`);
    await roleLink("billing_editor");
    expect(screen.getByLabelText("Project")).toHaveDisplayValue("Billing app");
    expect(screen.getByLabelText("Database")).toHaveDisplayValue("billing-prod");
    expect(screen.getByLabelText("Branch")).toHaveDisplayValue("main · protected");
    expect(search(m).view).toBeUndefined();
    // Never a request against something the picker dropped.
    expect(calls(m.f, "GET", `/v1/databases/${D2}/branches/${B2()}/roles`)).toHaveLength(0);
    expect(calls(m.f, "GET", `/v1/databases/${D1B}/branches/${B1B}/roles`)).toHaveLength(0);
  });

  it("says_a_protected_branch_is_protected", async () => {
    mountTab();
    expect(await screen.findByText(/main is protected: role and privilege changes here need Admin/)).toBeInTheDocument();
    expect(screen.getByText(/Roles live on this branch only/)).toBeInTheDocument();
  });
});

const B2 = () => "b0000000-0000-4000-8000-000000000002";

describe("role list", () => {
  it("lists_the_branchs_roles_but_never_the_owner_reserved_names_or_personal_logins", async () => {
    mountTab();
    for (const name of VISIBLE_ROLES) expect(await roleLink(name)).toBeInTheDocument();
    for (const name of HIDDEN_ROLES) expect(screen.queryByRole("link", { name: new RegExp(`^${name}\\b`) })).toBeNull();
    expect(screen.getByText(/7 roles on main/)).toBeInTheDocument();
  });

  it("shows_login_or_nologin_and_owner_level", async () => {
    mountTab();
    const reader = await roleLink("billing_reader");
    expect(reader).toHaveTextContent("NOLOGIN");
    expect(reader).not.toHaveTextContent(/owner-level/);
    expect(await roleLink("billing_admin")).toHaveTextContent(/owner-level/);
    const migrator = await roleLink("migrator");
    expect(migrator).toHaveTextContent("LOGIN");
    expect(migrator).not.toHaveTextContent("NOLOGIN");
  });

  it("each_role_links_to_its_page_with_the_postgres_database", async () => {
    mountTab();
    const link = await roleLink("billing_editor");
    expect(link).toHaveAttribute("href", `/app/members/roles/${D1}/${B1}/billing_editor?pg_database=${PG}`);
  });

  it("shows_the_name_escaped_and_keeps_the_real_name_in_the_link", async () => {
    mountTab();
    const link = await screen.findByRole("link", { name: (n) => n.includes(BIDI_ESCAPED) });
    expect(link).toHaveAttribute("href", expect.stringContaining(encodeURIComponent(BIDI)));
    expect(text()).not.toContain("‮");
  });

  it("defaults_to_the_list_and_switches_to_cards_remembering_the_choice", async () => {
    const m = mountTab();
    await roleLink("billing_editor");
    expect(screen.getByRole("table")).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "List" })).toBeChecked();
    await m.user.click(screen.getByRole("radio", { name: "Cards" }));
    expect(screen.queryByRole("table")).toBeNull();
    expect(await roleLink("billing_editor")).toBeInTheDocument();
    expect(search(m).view).toBe("cards");
    expect(localStorage.getItem("chelabase.members.view.roles")).toBe("cards");
  });

  it("starts_in_cards_when_the_url_or_the_stored_choice_says_so", async () => {
    localStorage.setItem("chelabase.members.view.roles", "cards");
    mountTab();
    await roleLink("billing_editor");
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.getByRole("radio", { name: "Cards" })).toBeChecked();
  });

  it("filters_by_name", async () => {
    const m = mountTab();
    await roleLink("billing_editor");
    await m.user.type(screen.getByRole("searchbox", { name: "Filter roles" }), "billing_r");
    expect(await roleLink("billing_reader")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /^billing_editor/ })).toBeNull();
  });

  it("says_when_the_roles_cannot_be_loaded", async () => {
    mountTab({ routes: { [`GET ${ROLES_URL}`]: apiError(500, "internal", "boom") } });
    expect(await screen.findByRole("alert")).toHaveTextContent(/Couldn't load roles/);
  });
});

describe("member counts are for Admins only (MH-34)", () => {
  it("a_tenant_admin_sees_counts_computed_from_the_projects_grants_chain_included", async () => {
    mountTab();
    // Sofia and (through billing_admin, a member of billing_editor) Ana.
    expect(await roleLink("billing_editor")).toHaveTextContent("2 members");
    expect(await roleLink("billing_reader")).toHaveTextContent("2 members");
    expect(await roleLink("billing_admin")).toHaveTextContent("1 member");
    expect(await roleLink("support_agent")).toHaveTextContent("No members");
  });

  it("a_project_admin_sees_them_too_and_the_list_has_a_members_column", async () => {
    mountTab({ caller: "project_admin" });
    expect(await roleLink("billing_editor")).toHaveTextContent("2 members");
    expect(within(screen.getByRole("table")).getByRole("columnheader", { name: /Members/ })).toBeInTheDocument();
  });

  it("a_developer_sees_roles_without_counts_and_the_grants_are_never_requested", async () => {
    const m = mountTab({ caller: "developer" });
    expect(await roleLink("billing_editor")).not.toHaveTextContent(/member/i);
    expect(screen.queryByRole("columnheader", { name: /Members/ })).toBeNull();
    expect(calls(m.f, "GET", `/v1/projects/${P1}/grants`)).toHaveLength(0);
  });

  it("a_viewer_sees_no_counts_either", async () => {
    mountTab({ caller: "viewer" });
    expect(await roleLink("billing_editor")).not.toHaveTextContent(/member/i);
  });
});

describe("personal logins (R3, P24)", () => {
  it("an_admin_sees_the_count_of_active_logins_with_the_members_and_links_to_their_pages", async () => {
    const m = mountTab();
    const section = (await screen.findByRole("heading", { name: /Personal logins on billing-prod/ })).closest("section")!;
    expect(within(section).getByRole("heading")).toHaveTextContent(/Personal logins on billing-prod\s*3/);
    expect(within(section).getByText(/Members' own logins. They aren't roles you grant/)).toBeInTheDocument();
    // Names come from the roster; each links to the member's page.
    const ana = within(section).getByRole("link", { name: "Ana Ng" });
    expect(ana).toHaveAttribute("href", `/app/members/${ANA}`);
    expect(within(section).getByRole("link", { name: "Sofia Marchetti" })).toHaveAttribute("href", `/app/members/${SOFIA}`);
    expect(within(section).getByRole("link", { name: "Priya Raman" })).toHaveAttribute("href", `/app/members/${PRI}`);
    expect(within(section).getByText("sofia")).toBeInTheDocument();
    expect(within(section).getByText(/in 9 days/)).toBeInTheDocument();
    // The revoked login's member isn't counted.
    expect(within(section).queryByRole("link", { name: "Dev Dunn" })).toBeNull();
    expect(calls(m.f, "GET", `/v1/databases/${D1}/personal-logins`)).toHaveLength(1);
  });

  it("the_link_goes_to_the_member_page", async () => {
    const m = mountTab();
    await m.user.click(await screen.findByRole("link", { name: "Ana Ng" }));
    expect(await screen.findByText("member page")).toBeInTheDocument();
    expect(m.router.state.location.pathname).toBe(`/app/members/${ANA}`);
  });

  it("is_for_project_admins_and_tenant_admins_only_and_is_not_even_requested_for_others", async () => {
    const dev = mountTab({ caller: "developer" });
    await roleLink("billing_editor");
    expect(screen.queryByRole("heading", { name: /Personal logins/ })).toBeNull();
    expect(calls(dev.f, "GET", `/v1/databases/${D1}/personal-logins`)).toHaveLength(0);
  });

  it("follows_the_selected_database", async () => {
    const m = mountTab({}, `/app/members?tab=roles&database=${D1B}`);
    await waitFor(() => expect(calls(m.f, "GET", `/v1/databases/${D1B}/personal-logins`)).toHaveLength(1));
    expect(screen.queryByRole("link", { name: "Ana Ng" })).toBeNull();
  });

  it("a_login_role_name_alone_never_makes_a_card", async () => {
    mountTab({ roles: [role("chelabase_owner", { origin: "owner" }), role("only_personal", { origin: "personal", login: true })] });
    expect(await screen.findByText(/0 roles on main|No roles/)).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /only_personal/ })).toBeNull();
  });
});

describe("create role", () => {
  async function open(m: ReturnType<typeof mountTab>) {
    await roleLink("billing_editor");
    await m.user.click(screen.getByRole("button", { name: "Create role" }));
    return screen.findByRole("dialog", { name: "Create a role" });
  }

  it("is_for_admins_only", async () => {
    mountTab({ caller: "developer" });
    await roleLink("billing_editor");
    expect(screen.queryByRole("button", { name: "Create role" })).toBeNull();
  });

  it("a_project_admin_gets_the_dialog_naming_where_the_role_will_live", async () => {
    const m = mountTab({ caller: "project_admin" });
    const dialog = await open(m);
    expect(within(dialog).getByText("On Billing app · billing-prod · main. This branch only.")).toBeInTheDocument();
    expect(within(dialog).getByText(/Lowercase letters, digits and _, starting with a letter or _. Up to 63 characters./)).toBeInTheDocument();
  });

  it("defaults_to_a_nologin_group_role_with_the_login_option_one_click_away", async () => {
    const m = mountTab();
    const dialog = await open(m);
    expect(within(dialog).getByRole("radio", { name: /Group role \(NOLOGIN\)/ })).toBeChecked();
    expect(within(dialog).getByRole("radio", { name: /Login role/ })).not.toBeChecked();
    expect(within(dialog).getByText("Members act as it. Nobody signs in as it.")).toBeInTheDocument();
  });

  it("keeps_the_advanced_options_closed_until_asked_and_has_no_owner_level_switch", async () => {
    const m = mountTab();
    const dialog = await open(m);
    const advanced = within(dialog).getByRole("button", { name: "Advanced" });
    expect(advanced).toHaveAttribute("aria-expanded", "false");
    expect(within(dialog).queryByLabelText("Connection limit")).toBeNull();
    await m.user.click(advanced);
    expect(advanced).toHaveAttribute("aria-expanded", "true");
    for (const label of ["Inherit privileges of the roles it's in", "Bypass row-level security", "Connection limit", "Valid until"]) {
      expect(within(dialog).getByLabelText(label)).toBeInTheDocument();
    }
    expect(within(dialog).getByRole("group", { name: "Member of" })).toBeInTheDocument();
    // Owner-level stays in the branch Roles panel (spec Part 4).
    expect(within(dialog).queryByRole("checkbox", { name: /^Owner-level$/i })).toBeNull();
    expect(within(dialog).queryByRole("switch", { name: /owner-level/i })).toBeNull();
  });

  it("offers_only_roles_a_new_role_may_join", async () => {
    const m = mountTab();
    const dialog = await open(m);
    await m.user.click(within(dialog).getByRole("button", { name: "Advanced" }));
    const group = within(dialog).getByRole("group", { name: "Member of" });
    const names = within(group).getAllByRole("checkbox").map((c) => c.getAttribute("aria-label") ?? c.closest("label")?.textContent ?? "");
    for (const ok of ["billing_reader", "support_agent"]) expect(names.some((n) => n.includes(ok))).toBe(true);
    for (const no of ["chelabase_owner", "authenticator", "chelabase_schema_locked", "sofia"]) expect(names.some((n) => n.includes(no))).toBe(false);
  });

  it("creates_the_role_and_goes_to_its_page", async () => {
    const created = role("refund_auditor");
    const m = mountTab({ routes: { [`POST ${ROLES_URL}`]: ok(created, 201) } });
    const dialog = await open(m);
    await m.user.type(within(dialog).getByLabelText("Name"), "refund_auditor");
    await m.user.click(within(dialog).getByRole("button", { name: "Advanced" }));
    await m.user.click(within(dialog).getByRole("checkbox", { name: /billing_reader/ }));
    await m.user.click(within(dialog).getByRole("button", { name: "Create role" }));
    expect(await screen.findByText(`role page route: refund_auditor on ${D1}/${B1}`)).toBeInTheDocument();
    const [body] = await bodies(m.f, "POST", ROLES_URL);
    expect(body).toMatchObject({ name: "refund_auditor", login: false, member_of: ["billing_reader"] });
    expect(body.owner_level).not.toBe(true);
    expect(m.router.state.location.search).toMatchObject({ pg_database: PG });
  });

  it("a_login_role_asks_for_login", async () => {
    const m = mountTab({ routes: { [`POST ${ROLES_URL}`]: ok(role("svc", { login: true }), 201) } });
    const dialog = await open(m);
    await m.user.type(within(dialog).getByLabelText("Name"), "svc");
    await m.user.click(within(dialog).getByRole("radio", { name: /Login role/ }));
    await m.user.click(within(dialog).getByRole("button", { name: "Create role" }));
    await screen.findByText(/role page route: svc/);
    expect((await bodies(m.f, "POST", ROLES_URL))[0]).toMatchObject({ name: "svc", login: true });
  });

  it("refuses_a_name_the_api_would_refuse_without_a_request", async () => {
    const m = mountTab();
    const dialog = await open(m);
    await m.user.type(within(dialog).getByLabelText("Name"), "Bad-Name");
    expect(within(dialog).getByRole("button", { name: "Create role" })).toBeDisabled();
    expect(calls(m.f, "POST", ROLES_URL)).toHaveLength(0);
  });

  it("shows_the_servers_refusal_and_stays_in_the_dialog", async () => {
    const m = mountTab({ routes: { [`POST ${ROLES_URL}`]: apiError(409, "role_exists", "a role named refund_auditor already exists") } });
    const dialog = await open(m);
    await m.user.type(within(dialog).getByLabelText("Name"), "refund_auditor");
    await m.user.click(within(dialog).getByRole("button", { name: "Create role" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("a role named refund_auditor already exists");
    expect(screen.getByRole("dialog", { name: "Create a role" })).toBeInTheDocument();
    expect(m.router.state.location.pathname).toBe("/app/members");
  });

  it("cancel_closes_without_a_request", async () => {
    const m = mountTab();
    const dialog = await open(m);
    await m.user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(calls(m.f, "POST", ROLES_URL)).toHaveLength(0);
  });
});
