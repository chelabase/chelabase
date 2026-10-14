import { describe, expect, it } from "vitest";
import fc from "fast-check";
import type { Operation, TypeSpec } from "@chelabase/management-client";
import {
  changesetKey, editIndexOfOp, editLabel, fold, foldAll, newEdit, opsOf, project, tableFingerprint, typeSpecText,
  type Edit, type ProjectedTable,
} from "./changeset";
import { invoices } from "./test-tables";

const T = { schema: "public", name: "invoices" };
const INT8: TypeSpec = { schema: "pg_catalog", name: "int8" };
const TEXT: TypeSpec = { schema: "pg_catalog", name: "text" };
const add = (name: string, data_type: TypeSpec = INT8, over: Record<string, unknown> = {}): Operation => ({ add_column: { table: T, column: { name, data_type, ...over } } });
const alter = (column: string, change: Record<string, unknown>): Operation => ({ alter_column: { table: T, column, change } });
const rename = (column: string, to: string): Operation => ({ rename_column: { table: T, column, to } });
const drop = (column: string): Operation => ({ drop_column: { table: T, column } });
const e = (...ops: Operation[]): Edit => newEdit(ops);
const opsAfter = (...steps: Operation[][]) => opsOf(steps.reduce<Edit[]>((acc, ops) => fold(invoices(), acc, e(...ops)), []));
const col = (p: ProjectedTable, name: string) => p.columns.find((c) => c.name === name);

describe("labels and keys", () => {
  it("changeset_key_joins_the_context", () => {
    expect(changesetKey({ databaseId: "d1", branchId: "b1", pgDatabase: "app", schema: "public", table: "invoices" })).toBe("d1/b1/app/public.invoices");
  });
  it("type_spec_text_omits_only_pg_catalog", () => {
    expect(typeSpecText(INT8)).toBe("int8");
    expect(typeSpecText({ schema: "public", name: "invoice_status" })).toBe("public.invoice_status");
    expect(typeSpecText({ ...TEXT, array: true })).toBe("text[]");
  });
  it("edit_labels", () => {
    expect(editLabel(add("discount_cents"))).toBe("Add column discount_cents int8");
    expect(editLabel(alter("status", { not_null: false }))).toBe("Change column status");
    expect(editLabel(rename("memo", "note"))).toBe("Rename column memo → note");
    expect(editLabel(drop("memo"))).toBe("Drop column memo");
    expect(editLabel({ set_rls: { table: T, enabled: true, forced: false } })).toBe("Turn RLS on");
    expect(editLabel({ add_constraint: { table: T, constraint: { kind: { unique: { columns: ["memo"] } } } } })).toBe("Add unique constraint on invoices");
    expect(editLabel({ drop_table: { table: T } })).toBe("Drop table public.invoices");
    expect(editLabel({ create_enum: { schema: "public", name: "tier", values: ["a"] } })).toBe("Create enum public.tier");
    expect(newEdit([add("x"), { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["x"] } } } } }]).label).toBe("Add column x int8");
  });
  it("edit_index_of_op_maps_flattened_indexes", () => {
    const edits = [e(add("a"), add("b")), e(drop("memo"))];
    expect(opsOf(edits)).toHaveLength(3);
    expect(editIndexOfOp(edits, 0)).toBe(0);
    expect(editIndexOfOp(edits, 1)).toBe(0);
    expect(editIndexOfOp(edits, 2)).toBe(1);
    expect(editIndexOfOp(edits, 3)).toBe(-1);
  });
  it("fingerprint_ignores_key_order", () => {
    const a = invoices();
    const reversed = (v: unknown): unknown =>
      Array.isArray(v) ? v.map(reversed) : v && typeof v === "object" ? Object.fromEntries(Object.entries(v).reverse().map(([k, x]) => [k, reversed(x)])) : v;
    const b = reversed(a) as typeof a;
    expect(Object.keys(b)).toEqual(Object.keys(a).reverse());
    expect(tableFingerprint(b)).toBe(tableFingerprint(a));
    expect(tableFingerprint(invoices({ comment: "x" }))).not.toBe(tableFingerprint(a));
  });
});

describe("fold examples", () => {
  it("F1_edits_to_an_added_column_rewrite_its_add", () => {
    expect(opsAfter([add("d")], [alter("d", { not_null: true, default: { set: "0" } })], [rename("d", "discount")])).toEqual([
      add("discount", INT8, { not_null: true, default: "0" }),
    ]);
  });
  it("F2_dropping_an_added_column_removes_it_and_its_constraints", () => {
    const uniq: Operation = { add_constraint: { table: T, constraint: { kind: { unique: { columns: ["d"] } } } } };
    expect(opsAfter([add("d"), uniq], [drop("d")])).toEqual([]);
  });
  it("F3_alters_merge_and_vanish_when_back_to_base", () => {
    expect(opsAfter([alter("memo", { not_null: true })], [alter("memo", { comment: { set: "c" } })])).toEqual([
      alter("memo", { not_null: true, comment: { set: "c" } }),
    ]);
    expect(opsAfter([alter("memo", { not_null: true })], [alter("memo", { not_null: false })])).toEqual([]);
  });
  it("F3_a_type_change_is_never_folded_away", () => {
    expect(opsAfter([alter("memo", { data_type: INT8 })], [alter("memo", { data_type: TEXT })])).toEqual([alter("memo", { data_type: TEXT })]);
  });
  it("F4_renames_chain_and_cancel", () => {
    expect(opsAfter([rename("memo", "note")], [rename("note", "remark")])).toEqual([rename("memo", "remark")]);
    expect(opsAfter([rename("memo", "note")], [rename("note", "memo")])).toEqual([]);
  });
  it("F5_dropping_a_renamed_base_column_drops_it_by_its_base_name", () => {
    expect(opsAfter([rename("memo", "note")], [alter("note", { not_null: true })], [drop("note")])).toEqual([drop("memo")]);
  });
  it("F6_column_ops_rewrite_a_pending_create_table", () => {
    const create: Operation = { create_table: { table: { schema: "public", name: "payments" }, columns: [{ name: "id", data_type: INT8, not_null: true }], primary_key: ["id"] } };
    const P = { schema: "public", name: "payments" };
    const steps = [e(create), e({ add_column: { table: P, column: { name: "note", data_type: TEXT } } }), e({ rename_column: { table: P, column: "note", to: "memo" } })];
    const folded = steps.reduce<Edit[]>((acc, s) => fold(null, acc, s), []);
    expect(opsOf(folded)).toEqual([
      { create_table: { table: P, columns: [{ name: "id", data_type: INT8, not_null: true }, { name: "memo", data_type: TEXT }], primary_key: ["id"] } },
    ]);
    expect(folded[0].label).toBe("Create table public.payments");
  });
  it("a_rewritten_edit_keeps_its_position", () => {
    const folded = [e(drop("memo")), e(add("d")), e(alter("amount_cents", { not_null: false }))].reduce<Edit[]>((acc, x) => fold(invoices(), acc, x), []);
    const again = fold(invoices(), folded, e(rename("d", "discount")));
    expect(again.map((x) => x.label)).toEqual(["Drop column memo", "Add column discount int8", "Change column amount_cents"]);
  });
});

describe("project", () => {
  it("marks_added_changed_dropped_and_keeps_order", () => {
    const p = project(invoices(), [e(add("discount_cents")), e(rename("memo", "note")), e(drop("customer_id"))]);
    expect(p.columns.map((c) => [c.name, c.mark])).toEqual([
      ["id", null], ["customer_id", "dropped"], ["status", null], ["amount_cents", null], ["note", "changed"], ["discount_cents", "added"],
    ]);
    expect(col(p, "note")?.original).toBe("memo");
    expect(col(p, "discount_cents")).toMatchObject({ original: null, data_type: "int8", type_spec: INT8, type_oid: null });
  });
  it("constraints_carry_names_and_pending_marks", () => {
    const p = project(invoices(), [e({ add_constraint: { table: T, constraint: { name: "memo_key", kind: { unique: { columns: ["memo"] } } } } }), e({ drop_constraint: { table: T, name: "invoices_customer_id_fkey" } })]);
    expect(p.constraints.map((c) => [c.name, c.kind, c.columns, c.mark])).toEqual([
      ["invoices_pkey", "primary_key", ["id"], null],
      ["invoices_customer_id_fkey", "foreign_key", ["customer_id"], "dropped"],
      ["memo_key", "unique", ["memo"], "added"],
    ]);
  });
  it("rls_and_draft_tables", () => {
    expect(project(invoices({ rls_enabled: false }), [e({ set_rls: { table: T, enabled: true, forced: false } })]).rls_enabled).toBe(true);
    const draft = project(null, [e({ create_table: { table: { schema: "public", name: "payments" }, columns: [{ name: "id", data_type: INT8 }] } })]);
    expect(draft).toMatchObject({ schema: "public", name: "payments" });
    expect(draft.columns.map((c) => [c.name, c.mark])).toEqual([["id", "added"]]);
  });
});

// ---- invariants (property tests) ----
const POOL = ["a", "b", "c", "x", "id", "memo", "status"];
const DEFAULTS = [null, "0", "'x'"];
const COMMENTS = [null, "c1", "c2"];
const TYPES = [INT8, TEXT, { schema: "pg_catalog", name: "bool" }];
type Intent =
  | { k: "add"; n: number } | { k: "drop"; i: number } | { k: "rename"; i: number; to: number }
  | { k: "notnull"; i: number; v: boolean } | { k: "default"; i: number; v: number } | { k: "comment"; i: number; v: number } | { k: "type"; i: number; t: number };
const intent: fc.Arbitrary<Intent> = fc.oneof(
  fc.record({ k: fc.constant("add" as const), n: fc.nat(6) }),
  fc.record({ k: fc.constant("drop" as const), i: fc.nat(9) }),
  fc.record({ k: fc.constant("rename" as const), i: fc.nat(9), to: fc.nat(6) }),
  fc.record({ k: fc.constant("notnull" as const), i: fc.nat(9), v: fc.boolean() }),
  fc.record({ k: fc.constant("default" as const), i: fc.nat(9), v: fc.nat(2) }),
  fc.record({ k: fc.constant("comment" as const), i: fc.nat(9), v: fc.nat(2) }),
  fc.record({ k: fc.constant("type" as const), i: fc.nat(9), t: fc.nat(2) }),
);
/** Turns intents into valid raw edits against the table as it stands after the previous ones. */
function rawEdits(intents: Intent[]): Edit[] {
  const base = invoices();
  const out: Edit[] = [];
  for (const it of intents) {
    const live = project(base, out).columns.filter((c) => c.mark !== "dropped");
    const names = new Set(live.map((c) => c.name));
    const target = live.length ? live[("i" in it ? it.i : 0) % live.length].name : null;
    let op: Operation | null = null;
    if (it.k === "add") op = names.has(POOL[it.n]) ? null : add(POOL[it.n]);
    else if (!target) op = null;
    else if (it.k === "drop") op = drop(target);
    else if (it.k === "rename") op = names.has(POOL[it.to]) ? null : rename(target, POOL[it.to]);
    else if (it.k === "notnull") op = alter(target, { not_null: it.v });
    else if (it.k === "default") op = alter(target, { default: DEFAULTS[it.v] === null ? "drop" : { set: DEFAULTS[it.v] } });
    else if (it.k === "comment") op = alter(target, { comment: COMMENTS[it.v] === null ? "clear" : { set: COMMENTS[it.v] } });
    else op = alter(target, { data_type: TYPES[it.t] });
    if (op) out.push(e(op));
  }
  return out;
}
const RUNS = { numRuns: 400 };

describe("fold invariants", () => {
  it("folded_has_the_same_effect_as_raw", () => {
    fc.assert(fc.property(fc.array(intent, { maxLength: 14 }), (intents) => {
      const raw = rawEdits(intents);
      expect(project(invoices(), foldAll(invoices(), raw))).toEqual(project(invoices(), raw));
    }), RUNS);
  });
  it("fold_is_idempotent", () => {
    fc.assert(fc.property(fc.array(intent, { maxLength: 14 }), (intents) => {
      const once = foldAll(invoices(), rawEdits(intents));
      expect(opsOf(foldAll(invoices(), once))).toEqual(opsOf(once));
    }), RUNS);
  });
  it("folded_is_minimal", () => {
    fc.assert(fc.property(fc.array(intent, { maxLength: 14 }), (intents) => {
      const ops = opsOf(foldAll(invoices(), rawEdits(intents)));
      const added = new Set<string>();
      const altered = new Set<string>();
      const renamed = new Set<string>();
      for (const op of ops) {
        if ("add_column" in op) added.add(op.add_column.column.name);
        const c = "alter_column" in op ? op.alter_column.column : "rename_column" in op ? op.rename_column.column : "drop_column" in op ? op.drop_column.column : null;
        if (c !== null) expect(added.has(c)).toBe(false);
        if ("alter_column" in op) { expect(altered.has(c!)).toBe(false); altered.add(c!); }
        if ("rename_column" in op) { expect(renamed.has(c!)).toBe(false); renamed.add(c!); }
      }
    }), RUNS);
  });
});
