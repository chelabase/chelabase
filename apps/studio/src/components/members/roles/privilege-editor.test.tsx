import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { CHANGES, PRIV, ROLE, calls, changeBodies, facts, gr, grantees, mountPage, privData, type ChangeBody } from "./test-setup";

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

async function loaded(opts: Parameters<typeof mountPage>[0] = {}) {
  const m = mountPage(opts);
  await screen.findByRole("table", { name: "Tables and views in billing" });
  return m;
}
const box = (label: string) => screen.getByRole("checkbox", { name: label });
const pending = () => screen.getByRole("complementary", { name: "Pending changes" });
const pendingTexts = () => within(pending()).getAllByRole("listitem").map((li) => li.textContent?.replace(/^\d+/, "").trim());
const bodyText = () => (document.body.textContent ?? "").replace(/\s+/g, " ");
const applyDisabledOrAbsent = () => {
  const b = screen.queryByRole("button", { name: /^Apply/ });
  if (b) expect(b).toBeDisabled();
};
const same = (a: string[], b: string[]) => expect([...a].sort()).toEqual([...b].sort());

type Handler = (body: ChangeBody) => Response | Promise<Response>;
const onChanges = (fn: Handler): Record<string, FakeHandler> => ({ [`POST ${CHANGES}`]: async (req) => fn((await req.clone().json()) as ChangeBody) });
const statements = (sql: string[]) => sql.map((s, i) => ({ op_index: i, sql: s, executed: false, lock: "none" }));
const dryOk = (sql: string[] = [], extra: object = {}) =>
  jsonResponse({ committed: false, dry_run: true, would_succeed: true, data_api_exposure: [], statements: statements(sql), ...extra });
const applied = () => jsonResponse({ committed: true, dry_run: false, data_api_exposure: [], statements: [] });
const refuse = (status: number, code: string, message: string, extra: object = {}) => jsonResponse({ error: { code, message, ...extra } }, status);

const CUSTOMERS = { relation: { schema: "billing", name: "customers" } };

describe("staging", () => {
  it("checkboxes_stage_and_review_shows_the_dry_run_sql", async () => {
    const sql = [
      "GRANT DELETE ON billing.customers TO billing_editor;",
      "GRANT UPDATE (email) ON billing.customers TO billing_editor;",
      "GRANT EXECUTE ON FUNCTION billing.recalc_totals(uuid) TO billing_editor;",
    ];
    const m = await loaded({ routes: onChanges((b) => (b.dry_run ? dryOk(sql) : applied())) });
    await m.user.click(box("DELETE on customers"));
    await m.user.click(screen.getByRole("button", { name: "Columns of customers" }));
    await m.user.click(box("UPDATE on customers.email"));
    await m.user.click(box("EXECUTE on billing.recalc_totals(uuid)"));

    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 3");
    same(pendingTexts() as string[], [
      "GRANT DELETE ON billing.customers",
      "GRANT UPDATE (email) ON billing.customers",
      "GRANT EXECUTE ON billing.recalc_totals(uuid)",
    ]);
    expect(within(pending()).getByText(/Nothing is applied yet/)).toBeInTheDocument();
    // Staging sends nothing.
    expect(calls(m.f, "POST", CHANGES)).toHaveLength(0);
    expect(box("DELETE on customers")).toBeChecked();

    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    const panel = await screen.findByRole("complementary", { name: "Review changes" });
    for (const line of sql) expect(within(panel).getByText(line, { exact: false })).toBeInTheDocument();
    expect(within(panel).getByText(/Dry run passed/)).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: /^Apply 3 changes/ })).toBeEnabled();
    expect(within(panel).queryByText(/Exposes data through the data API/)).toBeNull();

    const [dry] = await changeBodies(m.f);
    expect(dry.dry_run).toBe(true);
    expect(dry.database).toBe("billing");
    // An owner run: the page never asks to act as a role.
    expect(dry.role ?? null).toBeNull();
    same(facts(dry.operations), ["grant delete billing.customers", "grant update(email) billing.customers", "grant-function 5002"]);
    for (const g of grantees(dry.operations)) expect(g).toEqual({ role: ROLE });
  });

  it("clicking_a_staged_cell_again_the_remove_control_or_discard_unstages_it", async () => {
    const m = await loaded();
    await m.user.click(box("DELETE on customers"));
    await m.user.click(box("DELETE on customers"));
    expect(box("DELETE on customers")).not.toBeChecked();
    expect(screen.queryByRole("button", { name: "Review changes" })).toBeNull();

    await m.user.click(box("DELETE on customers"));
    await m.user.click(box("TRUNCATE on invoice_lines"));
    await m.user.click(within(pending()).getByRole("button", { name: "Remove GRANT DELETE ON billing.customers" }));
    expect(box("DELETE on customers")).not.toBeChecked();
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 1");

    await m.user.click(within(pending()).getByRole("button", { name: "Discard" }));
    expect(box("TRUNCATE on invoice_lines")).not.toBeChecked();
    expect(screen.queryByRole("button", { name: "Review changes" })).toBeNull();
    expect(calls(m.f, "POST", CHANGES)).toHaveLength(0);
  });

  it("unchecking_a_held_privilege_stages_a_revoke", async () => {
    const m = await loaded();
    await m.user.click(box("INSERT on invoices"));
    expect(box("INSERT on invoices")).not.toBeChecked();
    expect(pendingTexts()).toEqual(["REVOKE INSERT ON billing.invoices"]);
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    same(facts((await changeBodies(m.f))[0].operations), ["revoke insert billing.invoices"]);
  });

  it("a_row_all_toggle_stages_every_offered_privilege_and_completes_a_mixed_row", async () => {
    const m = await loaded();
    await m.user.click(box("All privileges on invoice_lines"));
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 7");
    await m.user.click(box("All privileges on invoice_lines"));
    expect(screen.queryByRole("button", { name: "Review changes" })).toBeNull();

    // invoices holds SELECT and INSERT: the other five are staged, nothing is revoked.
    await m.user.click(box("All privileges on invoices"));
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 5");
    for (const p of ["UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"]) expect(box(`${p} on invoices`)).toBeChecked();
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    same(facts((await changeBodies(m.f))[0].operations), ["update", "delete", "truncate", "references", "trigger"].map((p) => `grant ${p} billing.invoices`));
  });

  it("a_column_toggle_stages_the_privilege_on_every_table_that_can_be_changed", async () => {
    const m = await loaded();
    await m.user.click(box("DELETE on every table in billing"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    const got = facts((await changeBodies(m.f))[0].operations);
    for (const t of ["invoices", "invoice_lines", "customers", "plans", "refunds"]) expect(got).toContain(`grant delete billing.${t}`);
    // payments is owned by a role the owner can't act as.
    expect(got.some((f) => f.includes("payments"))).toBe(false);
  });

  it("builds_the_ops_for_every_kind_of_object_in_both_directions", async () => {
    const m = await loaded();
    await m.user.click(box("CREATE on schema billing"));
    await m.user.click(box("USAGE on schema billing"));
    await m.user.click(box("DELETE on customers"));
    await m.user.click(box("SELECT on invoices_id_seq"));
    await m.user.click(box("USAGE on invoices_id_seq"));
    await m.user.click(box("EXECUTE on billing.recalc_totals(uuid)"));
    await m.user.click(box("EXECUTE on billing.issue_refund(uuid, numeric)"));
    await m.user.click(box("DELETE on new tables"));
    await m.user.click(box("SELECT on new tables"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    const [dry] = await changeBodies(m.f);
    same(facts(dry.operations), [
      "grant-schema create billing",
      "revoke-schema usage billing",
      "grant delete billing.customers",
      "grant-sequence select billing.invoices_id_seq",
      "revoke-sequence usage billing.invoices_id_seq",
      "grant-function 5002",
      "revoke-function 5001",
      "default grant delete tables billing for postgres",
      "default revoke select tables billing for postgres",
    ]);
    // Functions travel by OID with the identity the read gave (MH-27).
    const fn = dry.operations.map((o) => o.grant_function).find(Boolean) as { function: { oid: number; identity: string } };
    expect(fn.function).toMatchObject({ oid: 5002, identity: "billing.recalc_totals(uuid)" });
    const def = dry.operations.map((o) => o.alter_default_privileges).find(Boolean) as { for_role: string; object: string; schema: string; grantee: unknown };
    expect(def).toMatchObject({ for_role: "postgres", object: "tables", schema: "billing", grantee: { role: ROLE } });
  });

  it("revoking_what_neon_superuser_granted_is_a_plain_owner_revoke", async () => {
    const m = await loaded();
    await m.user.click(box("SELECT on plans"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    const [dry] = await changeBodies(m.f);
    same(facts(dry.operations), ["revoke select billing.plans"]);
    expect(dry.role ?? null).toBeNull();
  });

  it("all_rows_is_its_own_op_and_touches_no_acl", async () => {
    const m = await loaded();
    await m.user.click(screen.getByRole("switch", { name: "All rows on refunds" }));
    expect(screen.getByRole("switch", { name: "All rows on refunds" })).toBeChecked();
    expect(pendingTexts()).toEqual(["All rows on billing.refunds"]);
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    const [dry] = await changeBodies(m.f);
    same(facts(dry.operations), ["all-rows on billing.refunds"]);
    const op = dry.operations[0].set_all_rows as { role: string };
    expect(op.role).toBe(ROLE);
    expect(dry.expect_acl).toEqual([]);
  });

  it("every_rls_table_stages_only_the_tables_that_are_off_and_a_switch_turns_one_off", async () => {
    const m = await loaded();
    await m.user.click(screen.getByRole("button", { name: "All rows on every RLS table in billing" }));
    expect(screen.getByRole("switch", { name: "All rows on refunds" })).toBeChecked();
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 1");
    await m.user.click(screen.getByRole("switch", { name: "All rows on customers" }));
    expect(screen.getByRole("switch", { name: "All rows on customers" })).not.toBeChecked();
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    same(facts((await changeBodies(m.f))[0].operations), ["all-rows on billing.refunds", "all-rows off billing.customers"]);
  });

  it("a_second_review_sends_the_edits_made_after_going_back", async () => {
    const m = await loaded();
    await m.user.click(box("DELETE on customers"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await m.user.click(await within(await screen.findByRole("complementary", { name: "Review changes" })).findByRole("button", { name: "Back" }));
    await m.user.click(box("TRUNCATE on invoice_lines"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    const all = await changeBodies(m.f);
    expect(all).toHaveLength(2);
    same(facts(all[1].operations), ["grant delete billing.customers", "grant truncate billing.invoice_lines"]);
  });
});

describe("grant option (MH-29)", () => {
  it("grant_option_is_revoke_only", async () => {
    const m = await loaded();
    await m.user.click(screen.getByText("Revoke grant option"));
    expect(pendingTexts()).toEqual(["REVOKE GRANT OPTION FOR SELECT ON billing.invoices"]);
    // Only the option goes: the privilege stays checked.
    expect(box("SELECT on invoices")).toBeChecked();
    await m.user.click(box("UPDATE on invoices"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await screen.findByRole("complementary", { name: "Review changes" });
    const [dry] = await changeBodies(m.f);
    same(facts(dry.operations), ["grant update billing.invoices", "revoke-option select billing.invoices"]);
    // Only the revoke carries the option flag; no grant does.
    for (const op of dry.operations) {
      const [kind, inner] = Object.entries(op)[0] as [string, { grant: { with_grant_option?: boolean } }];
      expect(inner.grant.with_grant_option === true).toBe(kind === "revoke");
    }
    expect(facts(dry.operations).some((f) => f.startsWith("WITH-OPTION"))).toBe(false);
  });
});

describe("apply", () => {
  it("apply_sends_expect_acl_for_touched_objects", async () => {
    const data = privData();
    const m = await loaded({
      data,
      routes: onChanges((b) => {
        if (b.dry_run) return dryOk();
        // The change lands: customers now holds DELETE.
        const customers = data.objects.billing.find((o) => o.name === "customers")!;
        customers.grants = [...customers.grants, gr("DELETE")];
        customers.acl_hash = "h-customers-after";
        return applied();
      }),
    });
    await m.user.click(box("DELETE on customers"));
    await m.user.click(box("CREATE on schema billing"));
    await m.user.click(box("SELECT on invoices_id_seq"));
    await m.user.click(box("EXECUTE on billing.recalc_totals(uuid)"));
    await m.user.click(box("DELETE on new tables"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await m.user.click(await within(await screen.findByRole("complementary", { name: "Review changes" })).findByRole("button", { name: /^Apply 5 changes/ }));

    await waitFor(() => expect(calls(m.f, "POST", CHANGES)).toHaveLength(2));
    const [dry, apply] = await changeBodies(m.f);
    expect(apply.dry_run ?? false).toBe(false);
    expect(apply.database).toBe("billing");
    expect(apply.role ?? null).toBeNull();
    expect(apply.operations).toEqual(dry.operations);
    // Each touched object's hash from the read, in serde form; nothing untouched.
    const key = (e: { object: unknown; acl_hash: string }) => JSON.stringify([e.object, e.acl_hash]);
    same(apply.expect_acl.map(key), [
      { object: CUSTOMERS, acl_hash: "h-customers" },
      { object: { schema: { schema: "billing" } }, acl_hash: "h-schema-billing" },
      { object: { sequence: { schema: "billing", name: "invoices_id_seq" } }, acl_hash: "h-invoices_id_seq" },
      { object: { function: { oid: 5002 } }, acl_hash: "h-recalc_totals" },
      { object: { default_acl: { for_role: "postgres", object: "tables", schema: "billing" } }, acl_hash: "h-def-tables" },
    ].map(key));
    expect(dry.expect_acl.map(key).sort()).toEqual(apply.expect_acl.map(key).sort());

    // Applied: the staging is gone and the read is fresh.
    expect(await screen.findByText(/Applied 5 changes/)).toBeInTheDocument();
    await waitFor(() => expect(box("DELETE on customers")).toBeChecked());
    expect(screen.queryByRole("complementary", { name: "Pending changes" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Review changes" })).toBeNull();
  });

  it("one_apply_at_a_time", async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const m = await loaded({
      routes: {
        [`POST ${CHANGES}`]: async (req) => {
          const b = (await req.clone().json()) as ChangeBody;
          if (b.dry_run) return dryOk();
          await gate;
          return applied();
        },
      },
    });
    await m.user.click(box("DELETE on customers"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    const apply = await within(await screen.findByRole("complementary", { name: "Review changes" })).findByRole("button", { name: /^Apply 1 change/ });
    await m.user.dblClick(apply);
    release();
    await screen.findByText(/Applied 1 change/);
    expect((await changeBodies(m.f)).filter((b) => !b.dry_run)).toHaveLength(1);
  });
});

describe("data API exposure (P31)", () => {
  it("flags_a_review_that_exposes_data_through_the_data_api_with_the_chain", async () => {
    const m = await loaded({
      routes: onChanges(() =>
        dryOk(["GRANT SELECT ON billing.invoice_lines TO billing_editor;"], {
          data_api_exposure: [{ op_index: 0, grantee: ROLE, chain: ["authenticated", ROLE] }],
        }),
      ),
    });
    await m.user.click(box("SELECT on invoice_lines"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    const panel = await screen.findByRole("complementary", { name: "Review changes" });
    const flag = within(panel).getByText(/Exposes data through the data API\./);
    expect(flag.closest("p, div, li")).toHaveTextContent(/authenticated/);
    expect(flag.closest("p, div, li")).toHaveTextContent(new RegExp(ROLE));
    // A warning, not a refusal.
    expect(within(panel).getByRole("button", { name: /^Apply 1 change/ })).toBeEnabled();
  });

  it("exposes_data_api_shows_the_chain", async () => {
    const m = await loaded({
      routes: onChanges(() =>
        refuse(422, "exposes_data_api", `operation 0: every row for "${ROLE}" would reach the data API through authenticated → ${ROLE}; nothing was changed`, {
          op_index: 0,
          chain: ["authenticated", ROLE],
        }),
      ),
    });
    await m.user.click(screen.getByRole("switch", { name: "All rows on refunds" }));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(`All rows can't be turned on for ${ROLE}`);
    expect(alert).toHaveTextContent("authenticated");
    // Apply stays disabled.
    expect(screen.getByRole("button", { name: /^Apply/ })).toBeDisabled();
    // Back to editing: the change is still staged and can be removed.
    await m.user.click(screen.getByRole("button", { name: /^Back/ }));
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 1");
  });

  it("a_refusal_at_apply_time_shows_the_same_box_and_keeps_the_edits", async () => {
    const m = await loaded({
      routes: onChanges((b) =>
        b.dry_run ? dryOk() : refuse(422, "exposes_data_api", "operation 0: every row would reach the data API", { op_index: 0, chain: ["anon", ROLE] }),
      ),
    });
    await m.user.click(screen.getByRole("switch", { name: "All rows on refunds" }));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await m.user.click(await within(await screen.findByRole("complementary", { name: "Review changes" })).findByRole("button", { name: /^Apply 1 change/ }));
    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(`All rows can't be turned on for ${ROLE}`);
    expect(alert).toHaveTextContent("anon");
    applyDisabledOrAbsent();
  });
});

describe("409 privileges_not_applied (MH-24)", () => {
  const mismatch = (extra: object = {}) => ({
    object: { relation: { schema: "billing", name: "plans" } },
    privilege: "SELECT",
    grantee: ROLE,
    still_granted_by: "neon_superuser",
    column: null,
    wanted: false,
    ...extra,
  });
  async function stageRevoke(routes: Record<string, FakeHandler>) {
    const m = await loaded({ routes });
    await m.user.click(box("SELECT on plans"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    return m;
  }

  it("privileges_not_applied_names_the_grantor", async () => {
    const m = await stageRevoke(onChanges(() => refuse(409, "privileges_not_applied", "Postgres doesn't hold the privileges as asked; nothing was changed", { mismatches: [mismatch()] })));
    const panel = await screen.findByRole("complementary", { name: "Not applied" });
    const alert = within(panel).getByRole("alert");
    expect(alert).toHaveTextContent("SELECT");
    expect(alert).toHaveTextContent("billing.plans");
    expect(alert).toHaveTextContent("neon_superuser");
    expect(alert).toHaveTextContent("The whole change rolled back.");
    expect(within(panel).queryByRole("button", { name: /^Apply/ })).toBeNull();
    // The edits stay staged.
    await m.user.click(within(panel).getByRole("button", { name: "Back to editing" }));
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 1");
    expect(box("SELECT on plans")).not.toBeChecked();
  });

  it("names_a_column_and_every_mismatch_and_never_prints_a_missing_grantor_as_null", async () => {
    await stageRevoke(
      onChanges(() =>
        refuse(409, "privileges_not_applied", "nothing was changed", {
          mismatches: [
            mismatch({ object: CUSTOMERS, column: "email", privilege: "UPDATE", still_granted_by: "ledger_svc" }),
            mismatch({ privilege: "INSERT", still_granted_by: null, wanted: true }),
          ],
        }),
      ),
    );
    const alert = within(await screen.findByRole("complementary", { name: "Not applied" })).getByRole("alert");
    expect(alert).toHaveTextContent("billing.customers");
    expect(alert).toHaveTextContent("email");
    expect(alert).toHaveTextContent("ledger_svc");
    expect(alert).toHaveTextContent("INSERT");
    expect(alert.textContent).not.toMatch(/\bnull\b|undefined/);
  });

  it("handles_empty_mismatches_with_the_servers_message", async () => {
    const m = await stageRevoke(
      onChanges(() =>
        refuse(409, "privileges_not_applied", "operation 0: Postgres granted or revoked less than asked (the role it ran as lacks a grant option); nothing was changed", {
          op_index: 0,
          mismatches: [],
        }),
      ),
    );
    const panel = await screen.findByRole("complementary", { name: "Not applied" });
    const alert = within(panel).getByRole("alert");
    expect(alert).toHaveTextContent(/Postgres granted or revoked less than asked/);
    expect(alert).toHaveTextContent("The whole change rolled back.");
    expect(alert.textContent).not.toMatch(/\bnull\b|undefined/);
    await m.user.click(within(panel).getByRole("button", { name: "Back to editing" }));
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 1");
  });

  it("the_same_box_at_apply_time", async () => {
    const m = await loaded({ routes: onChanges((b) => (b.dry_run ? dryOk() : refuse(409, "privileges_not_applied", "nothing was changed", { mismatches: [mismatch()] }))) });
    await m.user.click(box("SELECT on plans"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await m.user.click(await within(await screen.findByRole("complementary", { name: "Review changes" })).findByRole("button", { name: /^Apply 1 change/ }));
    expect(within(await screen.findByRole("complementary", { name: "Not applied" })).getByRole("alert")).toHaveTextContent("neon_superuser");
  });
});

describe("409 privileges_changed_since_read (P10)", () => {
  it("a_stale_read_prompts_a_reload", async () => {
    const data = privData();
    let stale = true;
    const m = await loaded({
      data,
      routes: onChanges((b) =>
        stale
          ? refuse(409, "privileges_changed_since_read", "the privileges changed since they were read", { objects: [CUSTOMERS] })
          : b.dry_run
            ? dryOk()
            : applied(),
      ),
    });
    await m.user.click(box("DELETE on customers"));
    await m.user.click(box("CREATE on schema billing"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));

    await waitFor(() =>
      expect(bodyText()).toContain("Privileges on billing.customers changed since you opened this page. Reload to see them; your 2 changes are kept to re-apply."),
    );
    applyDisabledOrAbsent();

    // Someone else changed customers meanwhile: the reload reads the new hash and keeps the edits.
    const customers = data.objects.billing.find((o) => o.name === "customers")!;
    customers.acl_hash = "h-customers-v2";
    stale = false;
    const reads = calls(m.f, "GET", PRIV).length;
    await m.user.click(screen.getByRole("button", { name: "Reload privileges" }));
    await waitFor(() => expect(calls(m.f, "GET", PRIV).length).toBeGreaterThan(reads));
    await waitFor(() => expect(box("DELETE on customers")).toBeChecked());
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 2");

    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await m.user.click(await within(await screen.findByRole("complementary", { name: "Review changes" })).findByRole("button", { name: /^Apply 2 changes/ }));
    await waitFor(() => expect(calls(m.f, "POST", CHANGES).length).toBeGreaterThanOrEqual(3));
    const all = await changeBodies(m.f);
    const apply = all[all.length - 1];
    expect(apply.dry_run ?? false).toBe(false);
    expect(apply.expect_acl.find((e) => JSON.stringify(e.object) === JSON.stringify(CUSTOMERS))?.acl_hash).toBe("h-customers-v2");
  });

  it("names_every_object_that_changed", async () => {
    const m = await loaded({
      routes: onChanges(() =>
        refuse(409, "privileges_changed_since_read", "changed", { objects: [CUSTOMERS, { schema: { schema: "billing" } }, { function: { oid: 5002 } }] }),
      ),
    });
    await m.user.click(box("DELETE on customers"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await waitFor(() => expect(bodyText()).toMatch(/changed since you opened this page/));
    expect(bodyText()).toMatch(/billing\.customers/);
    expect(bodyText()).toMatch(/billing\.recalc_totals|function 5002|5002/);
    expect(bodyText()).toMatch(/your 1 changes? (is|are) kept to re-apply/);
  });

  it("the_same_prompt_at_apply_time", async () => {
    const m = await loaded({
      routes: onChanges((b) => (b.dry_run ? dryOk() : refuse(409, "privileges_changed_since_read", "changed", { objects: [CUSTOMERS] }))),
    });
    await m.user.click(box("DELETE on customers"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    await m.user.click(await within(await screen.findByRole("complementary", { name: "Review changes" })).findByRole("button", { name: /^Apply 1 change/ }));
    expect(await screen.findByRole("button", { name: "Reload privileges" })).toBeInTheDocument();
  });
});

describe("other failures", () => {
  it("a_server_error_is_shown_and_the_edits_stay", async () => {
    const m = await loaded({ routes: onChanges(() => refuse(500, "internal", "something went wrong")) });
    await m.user.click(box("DELETE on customers"));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("something went wrong");
    applyDisabledOrAbsent();
    await m.user.click(screen.getByRole("button", { name: /^Back/ }));
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 1");
  });
});
