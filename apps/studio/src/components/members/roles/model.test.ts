import { describe, expect, it } from "vitest";
import { ApiError, type ObjectPrivileges, type ProjectGrant } from "@chelabase/management-client";
import { editText, expectAclOf, holdersOf, indexPrivileges, managedRoles, operationOf, privilegeSummary, stillChanges, type Edit } from "./model";
import { failureOf } from "./use-privilege-editor";
import { BIDI, D1, billingObjects, gr, privData, role, ROLES_D1, VISIBLE_ROLES } from "./test-setup";

/** Edge cases of the Roles tab's and role page's model beyond the RED set (coder's own, task S3). */

const page = (objects: ObjectPrivileges[], truncated = false) => ({ objects, truncated });
const bare = (o: ObjectPrivileges): ObjectPrivileges => ({ ...o, grants: [], columns: o.columns.map((c) => ({ ...c, grants: [] })), owner: "billing_owner" });

describe("privilegeSummary: a card's one line", () => {
  it("groups_what_the_role_holds_by_kind_of_object", () => {
    const line = privilegeSummary(role("billing_editor"), [page(billingObjects())]);
    // invoices, customers, payments, plans hold table privileges; customers.display_name a column one.
    expect(line).toBe("SELECT, INSERT on 4 tables in billing · UPDATE on 1 column · USAGE on 1 sequence in billing · EXECUTE on 1 function in billing");
  });

  it("says_no_privileges_yet_for_a_role_that_holds_nothing_and_is_in_nothing", () => {
    expect(privilegeSummary(role("fresh"), [page(billingObjects().map(bare))])).toBe("No privileges yet");
    expect(privilegeSummary(role("fresh"), [])).toBe("No privileges yet");
  });

  it("falls_back_to_its_memberships", () => {
    expect(privilegeSummary(role("billing_admin", { member_of: ["billing_editor", "billing_reader"] }), [page([])])).toBe("Member of billing_editor, billing_reader");
  });

  it("names_views_alone_as_views_and_marks_a_truncated_read", () => {
    const view = { ...billingObjects().find((o) => o.name === "open_balances")!, grants: [gr("SELECT")] };
    expect(privilegeSummary(role("r"), [page([view], true)])).toBe("SELECT on 1 view+ in billing");
  });

  it("says_what_the_role_owns_and_escapes_names", () => {
    const owned = { ...bare(billingObjects()[0]), owner: "migrator", schema: "bill‮ing" };
    expect(privilegeSummary(role("migrator"), [page([owned])])).toBe("Owns 1 object in bill\\u202eing");
  });
});

describe("holdersOf (MH-34)", () => {
  const grant = (member_id: string, pg_roles: ProjectGrant["pg_roles"], overrides: ProjectGrant["overrides"] = []): ProjectGrant =>
    ({ member_id, platform_role: "developer", pg_roles, overrides }) as ProjectGrant;

  it("follows_set_memberships_but_not_a_membership_without_set", () => {
    const rows = [role("a", { member_of: ["b"] }), role("b"), role("c", { member_of: ["b"], member_no_set: ["b"] })];
    expect(holdersOf("b", [grant("m1", ["a"]), grant("m2", ["c"]), grant("m3", ["b"])], D1, rows)).toEqual([
      { memberId: "m1", via: "a" },
      { memberId: "m3", via: null },
    ]);
  });

  it("uses_the_databases_override_and_never_lists_an_admins_all", () => {
    const rows = [role("b"), role("x")];
    const narrowed = grant("m1", ["b"], [{ database_id: D1, pg_roles: ["x"], platform_role: null }]);
    const elsewhere = grant("m2", ["b"], [{ database_id: "other", pg_roles: ["x"], platform_role: null }]);
    const unnarrowed = grant("m4", ["b"], [{ database_id: D1, pg_roles: null, platform_role: "viewer" }]);
    expect(holdersOf("b", [narrowed, elsewhere, grant("m3", "all"), unnarrowed], D1, rows).map((h) => h.memberId)).toEqual(["m2", "m4"]);
  });
});

describe("the role list", () => {
  it("hides_the_owner_reserved_names_and_personal_logins_but_keeps_the_data_apis_roles", () => {
    const rows = [...ROLES_D1, role("anon", { origin: "sql" }), role("authenticated", { origin: "sql" }), role("pg_monitor", { origin: "sql" }), role("cb_m_x", { origin: "sql" })];
    expect(managedRoles(rows).map((r) => r.name)).toEqual([...VISIBLE_ROLES.slice(0, 6), BIDI, "anon", "authenticated"]);
  });
});

describe("edits, operations and expect_acl", () => {
  const data = privData();
  const index = indexPrivileges({ schemas: data.schemas, default_privileges: data.default_privileges }, data.objects.billing);
  const customers = { kind: "relation", schema: "billing", name: "customers" } as const;

  it("sends_a_default_acl_without_a_row_without_a_hash_and_each_object_once", () => {
    const edits: Edit[] = [
      { kind: "privilege", target: customers, privilege: "DELETE", column: null, grant: true },
      { kind: "privilege", target: customers, privilege: "INSERT", column: "email", grant: true },
      { kind: "privilege", target: { kind: "default", forRole: "billing_owner", schema: "billing", object: "sequences" }, privilege: "USAGE", column: null, grant: true },
      { kind: "all_rows", schema: "billing", name: "refunds", enabled: true },
    ];
    expect(expectAclOf(edits, index)).toEqual([{ object: { relation: { schema: "billing", name: "customers" } }, acl_hash: "h-customers" }]);
  });

  it("drops_an_edit_the_read_already_matches_and_one_on_an_object_that_is_gone", () => {
    expect(stillChanges(index, { kind: "privilege", target: customers, privilege: "SELECT", column: null, grant: true })).toBe(false);
    expect(stillChanges(index, { kind: "privilege", target: customers, privilege: "SELECT", column: null, grant: false })).toBe(true);
    expect(stillChanges(index, { kind: "privilege", target: { ...customers, name: "gone" }, privilege: "SELECT", column: null, grant: true })).toBe(false);
    expect(stillChanges(index, { kind: "option", target: customers, privilege: "SELECT" })).toBe(false);
    expect(stillChanges(index, { kind: "all_rows", schema: "billing", name: "customers", enabled: true })).toBe(false);
  });

  it("revokes_only_the_grant_option_on_every_kind_and_never_grants_one", () => {
    const role = "billing_editor";
    const targets = [
      customers,
      { kind: "schema", schema: "billing" },
      { kind: "sequence", schema: "billing", name: "invoices_id_seq" },
      { kind: "function", schema: "billing", oid: 5001, identity: "billing.issue_refund(uuid, numeric)" },
    ] as const;
    expect(targets.map((target) => operationOf({ kind: "option", target, privilege: "USAGE" }, role))).toEqual([
      { revoke: { table: { schema: "billing", name: "customers" }, grant: { grantee: { role }, privileges: ["usage"], with_grant_option: true } } },
      { revoke_schema: { schema: "billing", grantee: { role }, privileges: ["usage"], grant_option_only: true } },
      { revoke_sequence: { sequence: { schema: "billing", name: "invoices_id_seq" }, grantee: { role }, privileges: ["usage"], grant_option_only: true } },
      { revoke_function: { function: { oid: 5001, identity: "billing.issue_refund(uuid, numeric)" }, grantee: { role }, grant_option_only: true } },
    ]);
    const grants = targets.map((target) => JSON.stringify(operationOf({ kind: "privilege", target, privilege: "SELECT", column: null, grant: true }, role)));
    for (const g of grants) expect(g).not.toMatch(/with_grant_option|grant_option_only/);
  });

  it("shows_names_escaped_in_the_edit_list", () => {
    expect(editText({ kind: "privilege", target: { kind: "relation", schema: "s", name: BIDI }, privilege: "SELECT", column: "c\u0000", grant: false })).toBe(
      "REVOKE SELECT (c\\u0000) ON s.refunds\\u202ereview",
    );
    expect(editText({ kind: "all_rows", schema: "s", name: "t", enabled: false })).toBe("All rows off on s.t");
  });
});

describe("failureOf", () => {
  it("keeps_only_well_formed_mismatches_and_objects", () => {
    const f = failureOf(
      new ApiError(409, "privileges_not_applied", "nothing was changed", {
        mismatches: [null, { object: "x", privilege: "SELECT" }, { object: { relation: { schema: "s", name: "t" } }, privilege: "SELECT", grantee: "r", still_granted_by: 7, wanted: "yes" }],
      }),
    );
    expect(f).toEqual({
      kind: "not_applied",
      message: "nothing was changed",
      mismatches: [{ object: { relation: { schema: "s", name: "t" } }, privilege: "SELECT", grantee: "r", still_granted_by: null, column: null, wanted: false }],
    });
    expect(failureOf(new ApiError(409, "privileges_changed_since_read", "changed", { objects: "nope" }))).toEqual({ kind: "stale", objects: [] });
    expect(failureOf(new ApiError(422, "exposes_data_api", "every row", { chain: ["anon", 3, "r"] }))).toEqual({ kind: "exposes", chain: ["anon", "r"], opIndex: null, message: "every row" });
    expect(failureOf(new Error("boom"))).toEqual({ kind: "error", message: "boom" });
  });
});
