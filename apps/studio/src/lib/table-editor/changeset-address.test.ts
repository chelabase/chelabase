import { describe, expect, it } from "vitest";
import fc from "fast-check";
import type { Operation, TableRef } from "@chelabase/management-client";
import { fold, foldAll, newEdit, opsOf, project, withoutEdit, type Edit } from "./changeset";
import { invoices } from "./test-tables";

// Final fix round, E-I1: every operation in a table changeset names the table as it is at that
// operation's position (after the earlier `rename_table` / `set_table_schema`), whichever name the
// caller staged it with (the base name or the current one), after fold, foldAll and withoutEdit.
const BASE: TableRef = { schema: "public", name: "invoices" };
const INT8 = { schema: "pg_catalog", name: "int8" };

const op = {
  rename: (table: TableRef, to: string): Operation => ({ rename_table: { table, to } }),
  move: (table: TableRef, schema: string): Operation => ({ set_table_schema: { table, schema } }),
  comment: (table: TableRef, comment: string): Operation => ({ set_table_comment: { table, comment } }),
  rls: (table: TableRef, enabled: boolean): Operation => ({ set_rls: { table, enabled, forced: false } }),
  add: (table: TableRef, name: string): Operation => ({ add_column: { table, column: { name, data_type: INT8 } } }),
  notNull: (table: TableRef, column: string): Operation => ({ alter_column: { table, column, change: { not_null: true } } }),
  unique: (table: TableRef, c: string): Operation => ({ add_constraint: { table, constraint: { kind: { unique: { columns: [c] } } } } }),
  selfFk: (table: TableRef, references: TableRef): Operation => ({
    add_constraint: { table, constraint: { kind: { foreign_key: { columns: ["customer_id"], references, referenced_columns: ["id"] } } } },
  }),
};

function body(o: Operation): Record<string, unknown> {
  return Object.values(o)[0] as Record<string, unknown>;
}

/** Fails unless each op names the table as it is at its position (and a self-referencing FK too). */
function expectAddressed(ops: Operation[]) {
  const cur = { ...BASE };
  const names = [`${BASE.schema}.${BASE.name}`];
  for (const o of ops) {
    const b = body(o);
    expect(b.table, JSON.stringify(o)).toEqual(cur);
    const fk = "add_constraint" in o && "foreign_key" in o.add_constraint.constraint.kind ? o.add_constraint.constraint.kind.foreign_key : null;
    if (fk && names.includes(`${fk.references.schema}.${fk.references.name}`)) expect(fk.references, JSON.stringify(o)).toEqual(cur);
    if ("rename_table" in o) cur.name = o.rename_table.to;
    if ("set_table_schema" in o) cur.schema = o.set_table_schema.schema;
    names.push(`${cur.schema}.${cur.name}`);
  }
}

function folded(...steps: Operation[][]): Edit[] {
  return steps.reduce<Edit[]>((acc, ops) => fold(invoices(), acc, newEdit(ops)), []);
}

const BILLS: TableRef = { schema: "public", name: "bills" };

describe("table addressing (E-I1)", () => {
  it("a_base_named_op_after_a_rename_names_the_new_table", () => {
    const edits = folded([op.rename(BASE, "bills")], [op.add(BASE, "d")], [op.rls(BASE, true)]);
    expectAddressed(opsOf(edits));
    expect(project(invoices(), edits).columns.some((c) => c.name === "d" && c.mark === "added")).toBe(true);
  });

  it("comment_rename_comment_keeps_the_comment_after_the_rename", () => {
    const edits = folded([op.comment(BASE, "A")], [op.rename(BASE, "bills")], [op.comment(BILLS, "B")]);
    const ops = opsOf(edits);
    expectAddressed(ops);
    expect(ops.filter((o) => "set_table_comment" in o)).toHaveLength(1);
    expect(project(invoices(), edits)).toMatchObject({ name: "bills", comment: "B" });
  });

  it("a_move_readdresses_later_ops", () => {
    const edits = folded([op.move(BASE, "billing")], [op.notNull(BASE, "memo")], [op.rename({ schema: "billing", name: "invoices" }, "bills")], [op.unique(BASE, "memo")]);
    expectAddressed(opsOf(edits));
  });

  it("removing_the_rename_readdresses_later_ops_to_the_base_name", () => {
    const r = newEdit([op.rename(BASE, "bills")]);
    const u = newEdit([op.unique(BILLS, "memo")]);
    const c = newEdit([op.comment(BILLS, "x")]);
    const left = withoutEdit(invoices(), [r, u, c], r.id);
    expect(left.map((e) => e.id)).toEqual([u.id, c.id]);
    expectAddressed(opsOf(left));
  });

  it("a_self_referencing_fk_named_by_the_base_name_follows_the_rename", () => {
    const edits = folded([op.rename(BASE, "bills")], [op.selfFk(BASE, BASE)]);
    expectAddressed(opsOf(edits));
    const fk = opsOf(edits).find((o) => "add_constraint" in o)!;
    expect(fk).toMatchObject({ add_constraint: { constraint: { kind: { foreign_key: { references: BILLS } } } } });
  });

  it("labels_name_the_table_the_op_addresses", () => {
    const edits = folded([op.rename(BASE, "bills")], [op.comment(BASE, "x")]);
    expect(edits.at(-1)!.label).toContain("bills");
  });
});

// The invariant over generated sequences: table renames and moves interleaved with column,
// constraint, comment and RLS operations, each staged with the base or the current name at random.
type Step =
  | { k: "rename"; to: number; useBase: boolean }
  | { k: "move"; to: number; useBase: boolean }
  | { k: "comment"; v: number; useBase: boolean }
  | { k: "rls"; v: boolean; useBase: boolean }
  | { k: "add"; n: number; useBase: boolean }
  | { k: "notnull"; useBase: boolean }
  | { k: "unique"; useBase: boolean };

const NAMES = ["bills", "ledger", "invoices", "dues"];
const SCHEMAS = ["public", "billing", "audit"];
const COLS = ["a", "b", "c"];

const step: fc.Arbitrary<Step> = fc.oneof(
  fc.record({ k: fc.constant("rename" as const), to: fc.nat(3), useBase: fc.boolean() }),
  fc.record({ k: fc.constant("move" as const), to: fc.nat(2), useBase: fc.boolean() }),
  fc.record({ k: fc.constant("comment" as const), v: fc.nat(2), useBase: fc.boolean() }),
  fc.record({ k: fc.constant("rls" as const), v: fc.boolean(), useBase: fc.boolean() }),
  fc.record({ k: fc.constant("add" as const), n: fc.nat(2), useBase: fc.boolean() }),
  fc.record({ k: fc.constant("notnull" as const), useBase: fc.boolean() }),
  fc.record({ k: fc.constant("unique" as const), useBase: fc.boolean() }),
);

function stage(edits: Edit[], s: Step): Edit[] {
  const p = project(invoices(), edits);
  const cur: TableRef = { schema: p.schema, name: p.name };
  const ref = s.useBase ? BASE : cur;
  const cols = new Set(p.columns.filter((c) => c.mark !== "dropped").map((c) => c.name));
  let ops: Operation[] | null = null;
  if (s.k === "rename") ops = NAMES[s.to] === p.name ? null : [op.rename(ref, NAMES[s.to])];
  else if (s.k === "move") ops = SCHEMAS[s.to] === p.schema ? null : [op.move(ref, SCHEMAS[s.to])];
  else if (s.k === "comment") ops = [op.comment(ref, `c${s.v}`)];
  else if (s.k === "rls") ops = [op.rls(ref, s.v)];
  else if (s.k === "add") ops = cols.has(COLS[s.n]) ? null : [op.add(ref, COLS[s.n])];
  else if (s.k === "notnull") ops = [op.notNull(ref, "memo")];
  else if (s.k === "unique") ops = p.constraints.some((c) => c.mark !== "dropped" && c.columns.join() === "memo" && c.kind === "unique") ? null : [op.unique(ref, "memo")];
  return ops ? fold(invoices(), edits, newEdit(ops)) : edits;
}

const RUNS = { numRuns: Number(process.env.FOLD_RUNS ?? 400) };

describe("table addressing invariant (E-I1)", () => {
  it(
    "every_op_names_the_table_at_its_position_after_fold_foldAll_and_withoutEdit",
    () => {
      fc.assert(
        fc.property(fc.array(step, { maxLength: 14 }), (steps) => {
          const edits = steps.reduce(stage, [] as Edit[]);
          expectAddressed(opsOf(edits));
          expectAddressed(opsOf(foldAll(invoices(), edits)));
          for (const e of edits) expectAddressed(opsOf(withoutEdit(invoices(), edits, e.id)));
        }),
        RUNS,
      );
    },
    60_000,
  );
});
