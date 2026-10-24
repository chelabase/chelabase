import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import {
  ANA, B1, BIDI, BIDI_ESCAPED, D1, P1, PG, PRIV, ROLE, ROLES_URL, SOFIA, apiError, bodies, calls, gr, mountPage, ok, privData, query, role, lastQuery,
} from "./test-setup";

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

/** The page after its first schema block has loaded. */
async function loaded(opts: Parameters<typeof mountPage>[0] = {}) {
  const m = mountPage(opts);
  await screen.findByRole("table", { name: "Tables and views in billing" });
  return m;
}
const matrix = () => screen.getByRole("table", { name: "Tables and views in billing" });
const rowOf = (name: string) => within(matrix()).getByRole("row", { name: new RegExp(`\\b${name}\\b`) });
const box = (label: string) => screen.getByRole("checkbox", { name: label });

describe("header", () => {
  it("names_the_role_where_it_lives_and_its_attributes", async () => {
    await loaded();
    expect(screen.getByRole("heading", { level: 1, name: ROLE })).toBeInTheDocument();
    const body = document.body.textContent ?? "";
    expect(body).toMatch(/Billing app.*billing-prod.*main.*protected.*Postgres database.*billing/s);
    for (const tag of ["NOLOGIN", "INHERIT", "no BYPASSRLS", "no connection limit"]) expect(screen.getByText(tag)).toBeInTheDocument();
    expect(screen.getByText("This branch only: to have it elsewhere, merge or reset the other branch.")).toBeInTheDocument();
  });

  it("links_back_to_the_roles_tab_on_the_same_branch", async () => {
    await loaded();
    const back = screen.getByRole("link", { name: /Roles/ });
    const url = new URL(back.getAttribute("href")!, "http://x");
    expect(url.pathname).toBe("/app/members");
    expect(Object.fromEntries(url.searchParams)).toMatchObject({ tab: "roles", project: P1, database: D1, branch: B1, pg_database: PG });
  });

  it("shows_a_login_role_and_a_limited_one_as_such", async () => {
    mountPage({ role: "migrator", roles: [role("chelabase_owner", { origin: "owner" }), role("migrator", { login: true, inherit: false, bypass_rls: true, connection_limit: 5 })] });
    await screen.findByRole("heading", { level: 1, name: "migrator" });
    for (const tag of ["LOGIN", "NOINHERIT", "BYPASSRLS", "connection limit 5"]) expect(screen.getByText(tag)).toBeInTheDocument();
  });

  it("shows_the_name_escaped", async () => {
    const m = mountPage({ role: BIDI, roles: [role("chelabase_owner", { origin: "owner" }), role(BIDI)] });
    expect(await screen.findByRole("heading", { level: 1, name: BIDI_ESCAPED })).toBeInTheDocument();
    expect(document.body.textContent).not.toContain("‮");
    // The request carries the real name.
    await waitFor(() => expect(calls(m.f, "GET", PRIV).length).toBeGreaterThan(0));
    expect(query(calls(m.f, "GET", PRIV)[0]).get("role")).toBe(BIDI);
  });

  it("says_when_the_role_is_not_on_the_branch", async () => {
    mountPage({ role: "ghost" });
    expect(await screen.findByText(/ghost.*not found|not found.*ghost/i)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /Roles/ })).toBeInTheDocument();
  });
});

describe("what is read", () => {
  it("reads_the_roles_privileges_in_the_main_postgres_database_when_none_is_named", async () => {
    const m = await loaded({ pgDatabase: null });
    const reads = calls(m.f, "GET", PRIV).map(query);
    expect(reads.every((q) => q.get("role") === ROLE)).toBe(true);
    expect(reads.some((q) => !q.has("schema"))).toBe(true);
    expect(reads.some((q) => q.get("schema") === PG)).toBe(true);
  });

  it("reads_the_named_postgres_database", async () => {
    const analytics = `/v1/databases/${D1}/branches/${B1}/pg-databases/analytics/privileges`;
    const data = privData();
    const m = mountPage({ pgDatabase: "analytics", routes: { [`GET ${analytics}`]: ok({ ...pagesOf(data), schemas: data.schemas }) } });
    await waitFor(() => expect(calls(m.f, "GET", analytics).length).toBeGreaterThan(0));
    expect(calls(m.f, "GET", PRIV)).toHaveLength(0);
  });

  it("opens_the_first_schema_and_reads_the_others_only_when_opened", async () => {
    const m = await loaded();
    const pub = screen.getByRole("button", { name: /^public\b/ });
    expect(pub).toHaveAttribute("aria-expanded", "false");
    expect(calls(m.f, "GET", PRIV).map(query).some((q) => q.get("schema") === "public")).toBe(false);
    await m.user.click(pub);
    expect(await screen.findByRole("table", { name: "Tables and views in public" })).toBeInTheDocument();
    expect(pub).toHaveAttribute("aria-expanded", "true");
    expect(lastQuery(m.f, "GET", PRIV)!.get("schema")).toBe("public");
  });

  it("loads_more_when_the_read_is_paged", async () => {
    const data = privData();
    const first = { ...pagesOf(data, PG), objects: data.objects[PG].slice(0, 2), next_after: "invoice_lines" };
    const second = { ...pagesOf(data, PG), objects: data.objects[PG].slice(2), next_after: null };
    const m = mountPage({
      data,
      routes: {
        [`GET ${PRIV}`]: (req) => {
          const q = new URL(req.url).searchParams;
          if (!q.has("schema")) return new Response(JSON.stringify(pagesOf(data)), { headers: { "Content-Type": "application/json" } });
          return new Response(JSON.stringify(q.get("after") === "invoice_lines" ? second : first), { headers: { "Content-Type": "application/json" } });
        },
      },
    });
    await screen.findByRole("table", { name: "Tables and views in billing" });
    expect(within(matrix()).queryByRole("row", { name: /\bcustomers\b/ })).toBeNull();
    await m.user.click(screen.getByRole("button", { name: /Load more/ }));
    expect(await within(matrix()).findByRole("row", { name: /\bcustomers\b/ })).toBeInTheDocument();
    expect(lastQuery(m.f, "GET", PRIV)!.get("after")).toBe("invoice_lines");
    expect(screen.queryByRole("button", { name: /Load more/ })).toBeNull();
  });
});

function pagesOf(data: ReturnType<typeof privData>, schema?: string) {
  return {
    role: ROLE,
    server_version_num: data.server_version_num,
    offered: data.offered,
    schemas: data.schemas,
    default_privileges: data.default_privileges,
    objects: schema ? data.objects[schema] : [],
    next_after: null,
  };
}

describe("schema block", () => {
  it("shows_usage_and_create_with_who_granted_usage", async () => {
    await loaded();
    expect(box("USAGE on schema billing")).toBeChecked();
    expect(box("CREATE on schema billing")).not.toBeChecked();
    // The grantor (postgres) isn't the schema's owner (billing_owner).
    expect(screen.getByText("USAGE granted by postgres")).toBeInTheDocument();
  });

  it("names_the_schemas_with_counts", async () => {
    await loaded();
    expect(screen.getByRole("button", { name: /^billing\b/ })).toHaveTextContent(/7 tables and views/);
    expect(screen.getByRole("button", { name: /^billing\b/ })).toHaveTextContent(/1 sequence/);
    expect(screen.getByRole("button", { name: /^billing\b/ })).toHaveTextContent(/3 functions/);
  });
});

describe("tables and views matrix", () => {
  it("has_a_column_per_offered_privilege_and_maintain_only_when_the_server_has_it", async () => {
    await loaded();
    const heads = within(matrix()).getAllByRole("columnheader").map((h) => h.textContent?.trim());
    expect(heads).toEqual(expect.arrayContaining(["Object", "SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER", "All", "Notes"]));
    expect(heads).not.toContain("MAINTAIN");
  });

  it("shows_maintain_on_a_server_that_offers_it", async () => {
    const data = privData();
    data.offered = { ...data.offered, table: [...data.offered.table, "maintain"] };
    await loaded({ data });
    expect(within(matrix()).getByRole("columnheader", { name: "MAINTAIN" })).toBeInTheDocument();
    expect(box("MAINTAIN on invoices")).not.toBeChecked();
  });

  it("checks_what_the_role_holds_and_nothing_else", async () => {
    await loaded();
    expect(box("SELECT on invoices")).toBeChecked();
    expect(box("INSERT on invoices")).toBeChecked();
    expect(box("UPDATE on invoices")).not.toBeChecked();
    expect(box("SELECT on invoice_lines")).not.toBeChecked();
    expect(box("SELECT on customers")).toBeChecked();
    // Column grants don't check a table-wide cell.
    expect(box("UPDATE on customers")).not.toBeChecked();
    expect(box("SELECT on open_balances")).not.toBeChecked();
  });

  it("marks_views_and_toggles_a_whole_row_or_column_with_a_mixed_state", async () => {
    await loaded();
    expect(rowOf("open_balances")).toHaveTextContent(/view/);
    expect(box("All privileges on invoices")).toBePartiallyChecked();
    expect(box("All privileges on invoice_lines")).not.toBeChecked();
    expect(box("SELECT on every table in billing")).toBePartiallyChecked();
    expect(box("DELETE on every table in billing")).not.toBeChecked();
  });

  it("notes_a_grant_option_and_offers_only_to_revoke_it", async () => {
    await loaded();
    expect(within(rowOf("invoices")).getByText(/SELECT with grant option/)).toBeInTheDocument();
    expect(screen.getAllByText("Revoke grant option")).toHaveLength(1);
    // MH-29: nothing grants with the option.
    expect(screen.queryByRole("checkbox", { name: /grant option/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /^(add|grant|with) .*grant option/i })).toBeNull();
    expect(screen.queryByText(/^with grant option$/i)).toBeNull();
  });

  it("says_who_granted_a_privilege_only_when_it_wasnt_the_owner", async () => {
    await loaded();
    expect(within(rowOf("plans")).getByText("SELECT granted by neon_superuser")).toBeInTheDocument();
    expect(within(rowOf("invoices")).queryByText(/granted by/)).toBeNull();
    expect(within(rowOf("customers")).queryByText(/granted by/)).toBeNull();
  });

  it("greys_a_row_owned_by_a_role_the_owner_cannot_act_as", async () => {
    await loaded();
    const row = rowOf("payments");
    expect(within(row).getByText("owned by ledger_svc: change in SQL as ledger_svc")).toBeInTheDocument();
    const boxes = within(row).getAllByRole("checkbox");
    expect(boxes.length).toBeGreaterThan(5);
    for (const b of boxes) expect(b).toBeDisabled();
    // The held privilege still shows.
    expect(within(row).getByRole("checkbox", { name: "SELECT on payments" })).toBeChecked();
    // A lock replaces the chevron.
    expect(within(row).queryByRole("button", { name: "Columns of payments" })).toBeNull();
    expect(within(rowOf("invoices")).getByRole("button", { name: "Columns of invoices" })).toBeInTheDocument();
  });

  it("greys_the_schema_too_when_its_owner_isnt_settable", async () => {
    const data = privData();
    data.schemas[0] = { ...data.schemas[0], owner: "ledger_svc", owner_settable: false };
    await loaded({ data });
    expect(box("USAGE on schema billing")).toBeDisabled();
    expect(box("CREATE on schema billing")).toBeDisabled();
    expect(screen.getAllByText("owned by ledger_svc: change in SQL as ledger_svc").length).toBeGreaterThan(0);
  });

  it("drills_into_columns_where_a_table_wide_privilege_covers_every_column", async () => {
    const m = await loaded();
    const toggle = screen.getByRole("button", { name: "Columns of customers" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("checkbox", { name: "UPDATE on customers.email" })).toBeNull();
    await m.user.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/Column privileges on customers. A table-wide privilege covers every column./)).toBeInTheDocument();
    // Table-wide SELECT: every column checked and fixed.
    for (const c of ["id", "email", "display_name", "tax_id"]) {
      expect(box(`SELECT on customers.${c}`)).toBeChecked();
      expect(box(`SELECT on customers.${c}`)).toBeDisabled();
    }
    expect(box("UPDATE on customers.display_name")).toBeChecked();
    expect(box("UPDATE on customers.display_name")).toBeEnabled();
    expect(box("UPDATE on customers.email")).not.toBeChecked();
    expect(box("INSERT on customers.email")).toBeEnabled();
    expect(screen.queryByRole("checkbox", { name: "DELETE on customers.email" })).toBeNull();
  });

  it("notes_column_grants_in_the_row", async () => {
    await loaded();
    expect(within(rowOf("customers")).getByText("1 column grant")).toBeInTheDocument();
  });
});

describe("all rows", () => {
  it("shows_a_switch_per_rls_table_with_the_policy_and_the_restrictive_policy_warning", async () => {
    await loaded();
    expect(screen.getByRole("switch", { name: "All rows on customers" })).toBeChecked();
    expect(screen.getByText("on: policy billing_editor_all_rows")).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "All rows on refunds" })).not.toBeChecked();
    // Tables without row-level security have no switch.
    expect(screen.queryByRole("switch", { name: "All rows on invoices" })).toBeNull();
    expect(screen.getByText(/Restrictive policies still apply, and a role without SELECT on a table sees nothing there./)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "All rows on every RLS table in billing" })).toBeInTheDocument();
  });
});

describe("sequences, functions and future objects", () => {
  it("lists_sequences_with_usage_select_update", async () => {
    await loaded();
    expect(screen.getByRole("heading", { name: "Sequences" })).toBeInTheDocument();
    expect(box("USAGE on invoices_id_seq")).toBeChecked();
    expect(box("SELECT on invoices_id_seq")).not.toBeChecked();
    expect(box("UPDATE on invoices_id_seq")).not.toBeChecked();
  });

  it("lists_functions_by_signature_and_greys_the_ones_owned_by_an_unsettable_role", async () => {
    await loaded();
    expect(screen.getByRole("heading", { name: "Functions and procedures" })).toBeInTheDocument();
    expect(screen.getByText(/billing\.issue_refund\(uuid, numeric\)/)).toBeInTheDocument();
    expect(box("EXECUTE on billing.issue_refund(uuid, numeric)")).toBeChecked();
    expect(box("EXECUTE on billing.recalc_totals(uuid)")).not.toBeChecked();
    expect(box("EXECUTE on billing.recalc_totals(uuid)")).toBeEnabled();
    expect(box("EXECUTE on billing.close_period(date)")).toBeDisabled();
    expect(screen.getAllByText("owned by ledger_svc: change in SQL as ledger_svc").length).toBeGreaterThan(0);
  });

  it("shows_default_privileges_for_the_owner_first_and_follows_the_created_by_select", async () => {
    const m = await loaded();
    expect(screen.getByRole("heading", { name: "Future objects" })).toBeInTheDocument();
    const by = screen.getByLabelText("Created by") as HTMLSelectElement;
    const values = within(by).getAllByRole("option").map((o) => (o as HTMLOptionElement).value);
    // The database owner first, then roles that own objects in the schema and can be acted as; never ledger_svc.
    expect(values[0]).toBe("postgres");
    expect(values).toContain("billing_owner");
    expect(values).not.toContain("ledger_svc");
    expect(by.value).toBe("postgres");
    expect(box("SELECT on new tables")).toBeChecked();
    expect(box("INSERT on new tables")).toBeChecked();
    expect(box("DELETE on new tables")).not.toBeChecked();
    expect(box("USAGE on new sequences")).not.toBeChecked();
    expect(box("EXECUTE on new functions")).not.toBeChecked();
    await m.user.selectOptions(by, "billing_owner");
    expect(box("SELECT on new tables")).not.toBeChecked();
  });
});

describe("read-only for people who can't edit", () => {
  it.each(["developer", "viewer"] as const)("a_%s_sees_the_matrix_without_controls_a_panel_or_holders", async (caller) => {
    const m = await loaded({ caller });
    for (const b of screen.getAllByRole("checkbox")) expect(b).toBeDisabled();
    for (const s of screen.getAllByRole("switch")) expect(s).toBeDisabled();
    expect(box("SELECT on invoices")).toBeChecked();
    expect(screen.queryByRole("button", { name: "Review changes" })).toBeNull();
    expect(screen.queryByRole("complementary", { name: "Pending changes" })).toBeNull();
    expect(screen.queryByRole("tab", { name: /^Members\b/ })).toBeNull();
    expect(calls(m.f, "GET", `/v1/projects/${P1}/grants`)).toHaveLength(0);
    await m.user.click(screen.getByRole("checkbox", { name: "DELETE on invoices" }));
    expect(box("DELETE on invoices")).not.toBeChecked();
  });
});

describe("Members, Memberships, Danger zone", () => {
  it("lists_the_tabs", async () => {
    await loaded();
    const tabs = screen.getAllByRole("tab").map((t) => t.textContent?.replace(/\s+/g, " ").trim());
    expect(tabs).toEqual(["Privileges", "Members 2", "Memberships", "Danger zone"]);
  });

  it("holders_are_admin_only_and_link_to_member_pages", async () => {
    const m = await loaded();
    await m.user.click(screen.getByRole("tab", { name: /^Members\b/ }));
    const panel = await screen.findByRole("tabpanel");
    expect(await within(panel).findByRole("link", { name: "Sofia Marchetti" })).toHaveAttribute("href", `/app/members/${SOFIA}`);
    const ana = within(panel).getByRole("link", { name: "Ana Ng" });
    expect(ana).toHaveAttribute("href", `/app/members/${ANA}`);
    // Ana holds it through billing_admin.
    expect(ana.closest("li, tr")).toHaveTextContent(/billing_admin/);
    expect(within(panel).queryByRole("link", { name: "Dev Dunn" })).toBeNull();
  });

  it("a_project_admin_sees_holders_too", async () => {
    await loaded({ caller: "project_admin" });
    expect(screen.getByRole("tab", { name: /^Members 2/ })).toBeInTheDocument();
  });

  it("memberships_shows_what_it_is_in_and_what_is_in_it", async () => {
    const m = mountPage({ role: "billing_admin" });
    await screen.findByRole("heading", { level: 1, name: "billing_admin" });
    await m.user.click(screen.getByRole("tab", { name: "Memberships" }));
    const panel = await screen.findByRole("tabpanel");
    expect(within(panel).getByRole("checkbox", { name: "billing_editor" })).toBeChecked();
    expect(within(panel).getByRole("checkbox", { name: "billing_reader" })).not.toBeChecked();
    expect(within(panel).queryByRole("checkbox", { name: "billing_admin" })).toBeNull();
    for (const hidden of ["chelabase_owner", "authenticator", "sofia"]) expect(within(panel).queryByRole("checkbox", { name: hidden })).toBeNull();
  });

  it("memberships_edit_through_patch_with_the_whole_new_list", async () => {
    const patch = `PATCH ${ROLES_URL}/billing_admin`;
    const m = mountPage({ role: "billing_admin", routes: { [patch]: ok(role("billing_admin", { member_of: ["billing_editor", "billing_reader"] })) } });
    await screen.findByRole("heading", { level: 1, name: "billing_admin" });
    await m.user.click(screen.getByRole("tab", { name: "Memberships" }));
    const panel = await screen.findByRole("tabpanel");
    await m.user.click(within(panel).getByRole("checkbox", { name: "billing_reader" }));
    await m.user.click(within(panel).getByRole("button", { name: "Save memberships" }));
    await waitFor(() => expect(calls(m.f, "PATCH", `${ROLES_URL}/billing_admin`)).toHaveLength(1));
    const [body] = await bodies(m.f, "PATCH", `${ROLES_URL}/billing_admin`);
    expect([...body.member_of].sort()).toEqual(["billing_editor", "billing_reader"]);
  });

  it("a_viewer_cannot_save_memberships", async () => {
    const m = mountPage({ role: "billing_admin", caller: "developer" });
    await screen.findByRole("heading", { level: 1, name: "billing_admin" });
    await m.user.click(screen.getByRole("tab", { name: "Memberships" }));
    expect(screen.queryByRole("button", { name: "Save memberships" })).toBeNull();
  });

  it("shows_the_servers_refusal_of_a_membership", async () => {
    const patch = `PATCH ${ROLES_URL}/billing_admin`;
    const m = mountPage({ role: "billing_admin", routes: { [patch]: apiError(422, "role_membership_not_allowed", "billing_reader isn't the owner's to grant") } });
    await screen.findByRole("heading", { level: 1, name: "billing_admin" });
    await m.user.click(screen.getByRole("tab", { name: "Memberships" }));
    const panel = await screen.findByRole("tabpanel");
    await m.user.click(within(panel).getByRole("checkbox", { name: "billing_reader" }));
    await m.user.click(within(panel).getByRole("button", { name: "Save memberships" }));
    expect(await within(panel).findByRole("alert")).toHaveTextContent("billing_reader isn't the owner's to grant");
  });
});

// A grant no one else reads: keeps `gr` imported for fixtures a test may extend.
void gr;
