import { describe, expect, it } from "vitest";
import fc from "fast-check";
import type { Operation, SchemaChangeResponse, Table, TableRef, TypeSpec } from "@chelabase/management-client";
import { fold, foldAll, newEdit, opsOf, project, type Edit } from "./changeset";
import { mapDryRun } from "./dry-run";
import { invoices } from "./test-tables";

// Fix round 1 of T1 (review findings I1, I2, M1, M3, M4). The invariant behind I1/I2/M4:
// folding never changes the effect of the changeset, where the effect includes constraint
// names (as Postgres generates them when the constraint is added) and FK targets, on base
// tables and on draft tables alike.

const T: TableRef = { schema: "public", name: "invoices" };
const INT8: TypeSpec = { schema: "pg_catalog", name: "int8" };
const TEXT: TypeSpec = { schema: "pg_catalog", name: "text" };
const add = (name: string, t: TableRef = T): Operation => ({ add_column: { table: t, column: { name, data_type: INT8 } } });
const alter = (column: string, change: Record<string, unknown>, t: TableRef = T): Operation => ({ alter_column: { table: t, column, change } });
const rename = (column: string, to: string, t: TableRef = T): Operation => ({ rename_column: { table: t, column, to } });
const drop = (column: string, t: TableRef = T): Operation => ({ drop_column: { table: t, column } });
const unique = (columns: string[], t: TableRef = T): Operation => ({ add_constraint: { table: t, constraint: { kind: { unique: { columns } } } } });
const fkSelf = (columns: string[], referenced_columns: string[], t: TableRef = T): Operation => ({
  add_constraint: { table: t, constraint: { kind: { foreign_key: { columns, references: t, referenced_columns } } } },
});
const dropCons = (name: string, t: TableRef = T): Operation => ({ drop_constraint: { table: t, name } });
const e = (...ops: Operation[]): Edit => newEdit(ops);
const foldSteps = (base: Table | null, ...steps: Operation[][]) => steps.reduce<Edit[]>((acc, ops) => fold(base, acc, e(...ops)), []);
const opsAfter = (...steps: Operation[][]) => opsOf(foldSteps(invoices(), ...steps));
const cons = (base: Table | null, edits: Edit[], name: string) => project(base, edits).constraints.find((c) => c.name === name);

describe("I1: generated constraint names are fixed when the constraint is added", () => {
  it("a_rename_after_a_pending_unique_keeps_its_generated_name", () => {
    const edits = foldSteps(invoices(), [unique(["memo"])], [rename("memo", "note")]);
    expect(cons(invoices(), edits, "invoices_memo_key")).toMatchObject({ kind: "unique", columns: ["note"], mark: "added" });
    expect(cons(invoices(), edits, "invoices_note_key")).toBeUndefined();
    expect(opsAfter([unique(["memo"])], [rename("memo", "note")], [dropCons("invoices_memo_key")])).toEqual([rename("memo", "note")]);
  });
  it("a_taken_name_gets_postgres_numeric_suffix", () => {
    const base = invoices({
      constraints: [...invoices().constraints, { name: "invoices_memo_key", kind: "unique", columns: [5], definition: "UNIQUE (memo)", deferrable: false, initially_deferred: false, oid: 9, references: null, validated: true }],
    } as Partial<Table>);
    const edits = foldSteps(base, [unique(["memo"])]);
    expect(project(base, edits).constraints.map((c) => [c.name, c.mark])).toContainEqual(["invoices_memo_key1", "added"]);
  });
  it("a_table_rename_after_a_pending_unique_keeps_its_name", () => {
    const edits = foldSteps(invoices(), [unique(["memo"])], [{ rename_table: { table: T, to: "bills" } }]);
    expect(project(invoices(), edits).constraints.map((c) => c.name)).toContain("invoices_memo_key");
  });
  it("on_a_column_added_here_the_name_follows_the_final_name", () => {
    const edits = foldSteps(invoices(), [add("d"), unique(["d"])], [rename("d", "e")]);
    expect(opsOf(edits)).toEqual([add("e"), unique(["e"])]);
    expect(cons(invoices(), edits, "invoices_e_key")).toMatchObject({ columns: ["e"], mark: "added" });
  });
});

describe("I2: self-referencing foreign keys follow renames and drops of added columns", () => {
  it("rename_rewrites_referenced_columns", () => {
    const steps = [[add("d")], [unique(["d"])], [fkSelf(["customer_id"], ["d"])], [rename("d", "e")]];
    expect(opsAfter(...steps)).toEqual([add("e"), unique(["e"]), fkSelf(["customer_id"], ["e"])]);
    const fk = project(invoices(), foldSteps(invoices(), ...steps)).constraints.find((c) => c.kind === "foreign_key" && c.mark === "added");
    expect(fk?.references).toEqual({ schema: "public", table: "invoices", columns: ["e"] });
  });
  it("drop_removes_a_foreign_key_that_references_the_added_column", () => {
    expect(opsAfter([add("d")], [unique(["d"])], [fkSelf(["customer_id"], ["d"])], [drop("d")])).toEqual([]);
  });
  it("base_foreign_keys_project_their_targets", () => {
    const fk = project(invoices(), []).constraints.find((c) => c.name === "invoices_customer_id_fkey");
    expect(fk?.references).toEqual({ schema: "public", table: "customers", columns: ["id"] });
  });
});

describe("M1: an alter after a rename stays the user's own edit, under the current name", () => {
  it("keeps_order_and_names", () => {
    const edits = foldSteps(invoices(), [rename("memo", "note")], [drop("status")], [alter("note", { not_null: true })]);
    expect(edits.map((x) => x.label)).toEqual(["Rename column memo → note", "Drop column status", "Change column note"]);
    expect(opsOf(edits)).toEqual([rename("memo", "note"), drop("status"), alter("note", { not_null: true })]);
    const more = fold(invoices(), edits, e(alter("note", { comment: { set: "c" } })));
    expect(opsOf(more)).toEqual([rename("memo", "note"), drop("status"), alter("note", { not_null: true, comment: { set: "c" } })]);
    expect(more[2].id).toBe(edits[2].id);
  });
});

describe("M3: apply follows would_succeed only", () => {
  const edits = [e(alter("memo", { data_type: TEXT }))];
  const stmt = { sql: "S", op_index: 0, executed: true, lock: "access_exclusive", notices: [], skipped: null };
  const view = (res: Record<string, unknown>) => mapDryRun(edits, { dry_run: true, committed: false, ...res } as SchemaChangeResponse, { table: "public.invoices", totalBytes: 100 });
  it("rewrites_and_dependents_are_information", () => {
    expect(view({ would_succeed: true, statements: [{ ...stmt, rewrites_table: true }] }).apply).toBe("ready");
    expect(view({ would_succeed: true, statements: [stmt], dependents: { hash: "h", objects: [{ kind: "view", name: "v", description: "view v" }] } }).apply).toBe("ready");
  });
  it("null_needs_an_acknowledgement_false_blocks", () => {
    expect(view({ would_succeed: null, statements: [stmt] }).apply).toBe("needs_ack");
    expect(view({ would_succeed: false, statements: [stmt] }).apply).toBe("blocked");
  });
});

// ---- the invariant, as property tests over columns AND constraints, on base and draft tables ----
const POOL = ["a", "b", "c", "x", "id", "memo", "status"];
type Intent =
  | { k: "add"; n: number } | { k: "drop"; i: number } | { k: "rename"; i: number; to: number }
  | { k: "notnull"; i: number; v: boolean } | { k: "type"; i: number; t: number }
  | { k: "unique"; i: number } | { k: "fkself"; i: number; j: number } | { k: "dropcons"; c: number };
const intent: fc.Arbitrary<Intent> = fc.oneof(
  fc.record({ k: fc.constant("add" as const), n: fc.nat(6) }),
  fc.record({ k: fc.constant("drop" as const), i: fc.nat(9) }),
  fc.record({ k: fc.constant("rename" as const), i: fc.nat(9), to: fc.nat(6) }),
  fc.record({ k: fc.constant("notnull" as const), i: fc.nat(9), v: fc.boolean() }),
  fc.record({ k: fc.constant("type" as const), i: fc.nat(9), t: fc.nat(1) }),
  fc.record({ k: fc.constant("unique" as const), i: fc.nat(9) }),
  fc.record({ k: fc.constant("fkself" as const), i: fc.nat(9), j: fc.nat(9) }),
  fc.record({ k: fc.constant("dropcons" as const), c: fc.nat(9) }),
);
function rawEdits(base: Table | null, t: TableRef, start: Edit[], intents: Intent[]): Edit[] {
  const out = [...start];
  for (const it of intents) {
    const p = project(base, out);
    const live = p.columns.filter((c) => c.mark !== "dropped");
    const names = new Set(live.map((c) => c.name));
    const pick = (i: number) => (live.length ? live[i % live.length].name : null);
    let op: Operation | null = null;
    if (it.k === "add") op = names.has(POOL[it.n]) ? null : add(POOL[it.n], t);
    else if (it.k === "dropcons") {
      const lc = p.constraints.filter((c) => c.mark !== "dropped");
      op = lc.length ? dropCons(lc[it.c % lc.length].name, t) : null;
    } else {
      const target = pick(it.i);
      if (!target) op = null;
      else if (it.k === "drop") op = live.length > 1 ? drop(target, t) : null;
      else if (it.k === "rename") op = names.has(POOL[it.to]) ? null : rename(target, POOL[it.to], t);
      else if (it.k === "notnull") op = alter(target, { not_null: it.v }, t);
      else if (it.k === "type") op = alter(target, { data_type: [INT8, TEXT][it.t] }, t);
      else if (it.k === "unique") op = unique([target], t);
      else op = fkSelf([target], [pick(it.j)!], t);
    }
    if (op) out.push(e(op));
  }
  return out;
}
const P: TableRef = { schema: "public", name: "payments" };
const draftStart = [e({ create_table: { table: P, columns: [{ name: "id", data_type: INT8, not_null: true }, { name: "memo", data_type: TEXT }], primary_key: ["id"] } })];
const RUNS = { numRuns: 400 };

describe("fold invariant with constraints", () => {
  it("base_table_folded_equals_raw_and_is_idempotent", () => {
    fc.assert(fc.property(fc.array(intent, { maxLength: 14 }), (intents) => {
      const raw = rawEdits(invoices(), T, [], intents);
      const once = foldAll(invoices(), raw);
      expect(project(invoices(), once)).toEqual(project(invoices(), raw));
      expect(opsOf(foldAll(invoices(), once))).toEqual(opsOf(once));
    }), RUNS);
  });
  it("draft_table_folded_equals_raw_and_is_idempotent", () => {
    fc.assert(fc.property(fc.array(intent, { maxLength: 14 }), (intents) => {
      const raw = rawEdits(null, P, draftStart, intents);
      const once = foldAll(null, raw);
      expect(project(null, once)).toEqual(project(null, raw));
      expect(opsOf(foldAll(null, once))).toEqual(opsOf(once));
      expect(opsOf(once).filter((op) => "create_table" in op)).toHaveLength(1);
    }), RUNS);
  });
});
