import { describe, expect, it } from "vitest";
import fc from "fast-check";
import type { Operation, TypeSpec } from "@chelabase/management-client";
import { fold, foldAll, newEdit, opsOf, project, tableFingerprint, type Edit } from "./changeset";
import { invoices } from "./test-tables";

// Wider invariant checks than the plan's: longer sequences, constraint and table-setting operations.
const T = { schema: "public", name: "invoices" };
const INT8: TypeSpec = { schema: "pg_catalog", name: "int8" };
const TEXT: TypeSpec = { schema: "pg_catalog", name: "text" };
const TYPES = [INT8, TEXT, { schema: "pg_catalog", name: "bool" }];
const POOL = ["a", "b", "c", "x", "id", "memo", "status"];
const DEFAULTS = [null, "0", "'x'"];
const COMMENTS = [null, "c1", "c2"];

type Intent =
  | { k: "add"; n: number; u: boolean }
  | { k: "drop"; i: number }
  | { k: "rename"; i: number; to: number }
  | { k: "notnull"; i: number; v: boolean }
  | { k: "default"; i: number; v: number }
  | { k: "comment"; i: number; v: number }
  | { k: "type"; i: number; t: number }
  | { k: "unique"; i: number }
  | { k: "check"; i: number }
  | { k: "dropcons"; i: number }
  | { k: "rls"; v: boolean };

const intent: fc.Arbitrary<Intent> = fc.oneof(
  fc.record({ k: fc.constant("add" as const), n: fc.nat(6), u: fc.boolean() }),
  fc.record({ k: fc.constant("drop" as const), i: fc.nat(9) }),
  fc.record({ k: fc.constant("rename" as const), i: fc.nat(9), to: fc.nat(6) }),
  fc.record({ k: fc.constant("notnull" as const), i: fc.nat(9), v: fc.boolean() }),
  fc.record({ k: fc.constant("default" as const), i: fc.nat(9), v: fc.nat(2) }),
  fc.record({ k: fc.constant("comment" as const), i: fc.nat(9), v: fc.nat(2) }),
  fc.record({ k: fc.constant("type" as const), i: fc.nat(9), t: fc.nat(2) }),
  fc.record({ k: fc.constant("unique" as const), i: fc.nat(9) }),
  fc.record({ k: fc.constant("check" as const), i: fc.nat(9) }),
  fc.record({ k: fc.constant("dropcons" as const), i: fc.nat(9) }),
  fc.record({ k: fc.constant("rls" as const), v: fc.boolean() }),
);

function rawEdits(intents: Intent[]): Edit[] {
  const base = invoices();
  const out: Edit[] = [];
  for (const it of intents) {
    const p = project(base, out);
    const live = p.columns.filter((c) => c.mark !== "dropped");
    const names = new Set(live.map((c) => c.name));
    const target = live.length ? live[("i" in it ? it.i : 0) % live.length].name : null;
    let ops: Operation[] | null = null;
    if (it.k === "add") {
      const n = POOL[it.n];
      if (!names.has(n)) {
        ops = [{ add_column: { table: T, column: { name: n, data_type: INT8 } } }];
        if (it.u) ops.push({ add_constraint: { table: T, constraint: { kind: { unique: { columns: [n] } } } } });
      }
    } else if (it.k === "rls") ops = [{ set_rls: { table: T, enabled: it.v, forced: false } }];
    else if (it.k === "dropcons") {
      const cons = p.constraints.filter((c) => c.mark !== "dropped");
      if (cons.length) ops = [{ drop_constraint: { table: T, name: cons[it.i % cons.length].name } }];
    } else if (!target) ops = null;
    else if (it.k === "drop") ops = [{ drop_column: { table: T, column: target } }];
    else if (it.k === "rename") ops = names.has(POOL[it.to]) ? null : [{ rename_column: { table: T, column: target, to: POOL[it.to] } }];
    else if (it.k === "notnull") ops = [{ alter_column: { table: T, column: target, change: { not_null: it.v } } }];
    else if (it.k === "default")
      ops = [{ alter_column: { table: T, column: target, change: { default: DEFAULTS[it.v] === null ? "drop" : { set: DEFAULTS[it.v]! } } } }];
    else if (it.k === "comment")
      ops = [{ alter_column: { table: T, column: target, change: { comment: COMMENTS[it.v] === null ? "clear" : { set: COMMENTS[it.v]! } } } }];
    else if (it.k === "type") ops = [{ alter_column: { table: T, column: target, change: { data_type: TYPES[it.t] } } }];
    else if (it.k === "unique") {
      const exists = p.constraints.some((c) => c.mark !== "dropped" && c.name === `invoices_${target}_key`);
      ops = exists ? null : [{ add_constraint: { table: T, constraint: { kind: { unique: { columns: [target] } } } } }];
    } else if (it.k === "check") {
      const exists = p.constraints.some((c) => c.mark !== "dropped" && c.name === `invoices_${target}_check`);
      ops = exists ? null : [{ add_constraint: { table: T, constraint: { kind: { check: { expression: `${target} is not null` } } } } }];
    }
    if (ops) out.push(newEdit(ops));
  }
  return out;
}

const RUNS = { numRuns: Number(process.env.FOLD_RUNS ?? 1500) };
const seq = fc.array(intent, { maxLength: 24 });

function checkMinimal(ops: Operation[]) {
  const added = new Set<string>();
  const altered = new Set<string>();
  const renamed = new Set<string>();
  for (const op of ops) {
    if ("add_column" in op) added.add(op.add_column.column.name);
    const c = "alter_column" in op ? op.alter_column.column : "rename_column" in op ? op.rename_column.column : "drop_column" in op ? op.drop_column.column : null;
    if (c !== null) expect(added.has(c)).toBe(false);
    if ("alter_column" in op) {
      expect(altered.has(c!)).toBe(false);
      altered.add(c!);
    }
    if ("rename_column" in op) {
      expect(renamed.has(c!)).toBe(false);
      renamed.add(c!);
    }
  }
}

const columnOnly = fc.array(
  intent.filter((it) => ["drop", "rename", "notnull", "default", "comment", "type"].includes(it.k) || (it.k === "add" && !it.u)),
  { maxLength: 24 },
);

describe("fingerprint", () => {
  it("ignores_key_order_at_every_depth", () => {
    const reverse = (v: unknown): unknown =>
      Array.isArray(v)
        ? v.map(reverse)
        : v !== null && typeof v === "object"
          ? Object.fromEntries(Object.entries(v).reverse().map(([k, x]) => [k, reverse(x)]))
          : v;
    const a = invoices();
    const b = reverse(a) as typeof a;
    expect(JSON.stringify(b)).not.toBe(JSON.stringify(a));
    expect(tableFingerprint(b)).toBe(tableFingerprint(a));
    expect(tableFingerprint(invoices({ comment: "x" }))).not.toBe(tableFingerprint(a));
  });
});

describe("fold invariants with constraints", () => {
  it("folded_has_the_same_effect_and_is_idempotent", () => {
    // Minimality isn't asserted here: in rare sequences (an added column taking a name another column frees
    // later, with pending constraints in between) folding would reorder pending constraints, so the fold keeps
    // the operation as given instead.
    fc.assert(
      fc.property(seq, (intents) => {
        const raw = rawEdits(intents);
        const once = foldAll(invoices(), raw);
        expect(project(invoices(), once)).toEqual(project(invoices(), raw));
        expect(opsOf(foldAll(invoices(), once))).toEqual(opsOf(once));
      }),
      RUNS,
    );
  });

  it("column_only_sequences_fold_to_minimal", () => {
    fc.assert(
      fc.property(columnOnly, (intents) => {
        const raw = rawEdits(intents);
        const once = foldAll(invoices(), raw);
        expect(project(invoices(), once)).toEqual(project(invoices(), raw));
        expect(opsOf(foldAll(invoices(), once))).toEqual(opsOf(once));
        checkMinimal(opsOf(once));
      }),
      RUNS,
    );
  });

  it("an_added_column_renamed_with_its_unique_keeps_them_together", () => {
    const edits = [
      newEdit([
        { add_column: { table: T, column: { name: "d", data_type: INT8 } } },
        { add_constraint: { table: T, constraint: { kind: { check: { expression: "d > 0 and \"d\" < 10" } } } } },
      ]),
    ];
    const out = fold(invoices(), edits, newEdit([{ rename_column: { table: T, column: "d", to: "discount" } }]));
    expect(opsOf(out)).toEqual([
      { add_column: { table: T, column: { name: "discount", data_type: INT8 } } },
      { add_constraint: { table: T, constraint: { kind: { check: { expression: "discount > 0 and discount < 10" } } } } },
    ]);
  });

  it("a_pending_constraint_dropped_again_disappears", () => {
    const edits = [newEdit([{ add_constraint: { table: T, constraint: { kind: { unique: { columns: ["memo"] } } } } }])];
    expect(fold(invoices(), edits, newEdit([{ drop_constraint: { table: T, name: "invoices_memo_key" } }]))).toEqual([]);
  });

  it("rls_back_to_base_disappears", () => {
    const on = foldAll(invoices({ rls_enabled: false }), [newEdit([{ set_rls: { table: T, enabled: true, forced: false } }])]);
    expect(fold(invoices({ rls_enabled: false }), on, newEdit([{ set_rls: { table: T, enabled: false, forced: false } }]))).toEqual([]);
  });

  it("an_alter_after_a_rename_goes_before_it_only_when_its_name_is_another_columns_alter", () => {
    // memo is altered, renamed to a; status takes the name memo and is altered: appending would alter "memo" twice.
    const steps: Operation[] = [
      { alter_column: { table: T, column: "memo", change: { not_null: true } } },
      { rename_column: { table: T, column: "memo", to: "a" } },
      { rename_column: { table: T, column: "status", to: "memo" } },
      { alter_column: { table: T, column: "memo", change: { comment: { set: "c" } } } },
    ];
    const out = foldAll(invoices(), steps.map((op) => newEdit([op])));
    expect(out.map((e) => e.label)).toEqual(["Change column memo", "Rename column memo → a", "Change column status", "Rename column status → memo"]);
  });
});
