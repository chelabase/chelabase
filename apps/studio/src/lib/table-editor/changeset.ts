import type {
  Column,
  ColumnChange,
  ColumnSpec,
  ConstraintSpec,
  Operation,
  Table,
  TableRef,
  TypeSpec,
} from "@chelabase/management-client";

/** One user action: the operations it generated, numbered in the changes panel. */
export interface Edit {
  id: string;
  ops: Operation[];
  label: string;
}

/** The pending structure changes of one table (or of a schema's types, or of a draft table). */
export interface Changeset {
  key: string;
  /** `tableFingerprint` of the table the edits were made against; null for a draft. */
  base: string | null;
  edits: Edit[];
}

export type Mark = "added" | "changed" | "dropped";

export interface ProjectedColumn {
  name: string; // current name (a dropped base column keeps its base name)
  original: string | null; // base name; null for a column added in this changeset
  type_oid: number | null; // base type oid; null when added or when a type change is pending
  type_spec: TypeSpec | null; // the pending/added type; null when unchanged
  data_type: string; // base data_type, or typeSpecText(type_spec)
  not_null: boolean;
  default: string | null;
  comment: string | null;
  mark: Mark | null;
}

export interface ProjectedConstraint {
  name: string;
  kind: "primary_key" | "unique" | "foreign_key" | "check" | "exclusion";
  columns: string[]; // column names
  definition: string | null; // base definition; null for a pending one
  validated: boolean;
  mark: Mark | null;
  references: { schema: string; table: string; columns: string[] } | null; // a foreign key's target; null for other kinds
}

export interface ProjectedTable {
  schema: string;
  name: string;
  comment: string | null;
  rls_enabled: boolean;
  dropped: boolean;
  columns: ProjectedColumn[]; // base columns in attnum order (dropped stay in place), then added ones in add order
  constraints: ProjectedConstraint[];
}

// ---------------------------------------------------------------------------------------------
// Keys, labels, small helpers
// ---------------------------------------------------------------------------------------------

export function changesetKey(c: { databaseId: string; branchId: string; pgDatabase: string; schema: string; table: string }): string {
  return `${c.databaseId}/${c.branchId}/${c.pgDatabase}/${c.schema}.${c.table}`;
}

/** "int8", "public.invoice_status", "numeric(10,2)", "text[]": only `pg_catalog` is left unqualified. */
export function typeSpecText(t: TypeSpec): string {
  const qualified = t.schema === "pg_catalog" ? t.name : `${t.schema}.${t.name}`;
  const mods = t.modifiers && t.modifiers.length > 0 ? `(${t.modifiers.join(",")})` : "";
  return `${qualified}${mods}${t.array ? "[]" : ""}`;
}

type ConstraintKindName = ProjectedConstraint["kind"];

const KIND_LABEL: Record<ConstraintKindName, string> = {
  primary_key: "primary key",
  unique: "unique constraint",
  foreign_key: "foreign key",
  check: "check constraint",
  exclusion: "exclusion constraint",
};

function opKey(op: Operation): string {
  return Object.keys(op)[0] ?? "";
}

function specKind(spec: ConstraintSpec): ConstraintKindName {
  return Object.keys(spec.kind)[0] as ConstraintKindName;
}

export function editLabel(op: Operation): string {
  if ("add_column" in op) return `Add column ${op.add_column.column.name} ${typeSpecText(op.add_column.column.data_type)}`;
  if ("alter_column" in op) return `Change column ${op.alter_column.column}`;
  if ("rename_column" in op) return `Rename column ${op.rename_column.column} → ${op.rename_column.to}`;
  if ("drop_column" in op) return `Drop column ${op.drop_column.column}`;
  if ("add_constraint" in op) return `Add ${KIND_LABEL[specKind(op.add_constraint.constraint)]} on ${op.add_constraint.table.name}`;
  if ("validate_constraint" in op) return `Validate ${op.validate_constraint.name}`;
  if ("rename_constraint" in op) return `Rename constraint ${op.rename_constraint.name} → ${op.rename_constraint.to}`;
  if ("drop_constraint" in op) return `Drop constraint ${op.drop_constraint.name}`;
  if ("set_rls" in op) return op.set_rls.enabled ? "Turn RLS on" : "Turn RLS off";
  if ("create_table" in op) return `Create table ${op.create_table.table.schema}.${op.create_table.table.name}`;
  if ("rename_table" in op) return `Rename table ${op.rename_table.table.name} → ${op.rename_table.to}`;
  if ("set_table_schema" in op) return `Move table ${op.set_table_schema.table.name} to ${op.set_table_schema.schema}`;
  if ("set_table_comment" in op) return `Change comment on ${op.set_table_comment.table.name}`;
  if ("drop_table" in op) return `Drop table ${op.drop_table.table.schema}.${op.drop_table.table.name}`;
  if ("create_enum" in op) return `Create enum ${op.create_enum.schema}.${op.create_enum.name}`;
  if ("add_enum_value" in op) return `Add value ${op.add_enum_value.value} to ${op.add_enum_value.name}`;
  if ("rename_enum_value" in op) return `Rename value ${op.rename_enum_value.from} → ${op.rename_enum_value.to} in ${op.rename_enum_value.name}`;
  if ("rename_enum" in op) return `Rename enum ${op.rename_enum.name} → ${op.rename_enum.to}`;
  if ("drop_enum" in op) return `Drop enum ${op.drop_enum.schema}.${op.drop_enum.name}`;
  return opKey(op);
}

export function newEdit(ops: Operation[]): Edit {
  return { id: crypto.randomUUID(), ops, label: ops.length > 0 ? editLabel(ops[0]) : "" };
}

export function opsOf(edits: Edit[]): Operation[] {
  return edits.flatMap((e) => e.ops);
}

/** The edit that holds the `opIndex`-th operation of `opsOf(edits)`; -1 when out of range. */
export function editIndexOfOp(edits: Edit[], opIndex: number): number {
  if (!Number.isInteger(opIndex) || opIndex < 0) return -1;
  let seen = 0;
  for (let i = 0; i < edits.length; i++) {
    seen += edits[i].ops.length;
    if (opIndex < seen) return i;
  }
  return -1;
}

function canonical(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(canonical);
  if (v !== null && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const k of Object.keys(v).sort()) out[k] = canonical((v as Record<string, unknown>)[k]);
    return out;
  }
  return v;
}

/** Canonical JSON of a `getTable` response (keys sorted recursively): equal tables give equal strings. */
export function tableFingerprint(t: Table): string {
  return JSON.stringify(canonical(t));
}

function clone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v)) as T;
}

// ---------------------------------------------------------------------------------------------
// Identifiers inside SQL expressions (check constraints)
// ---------------------------------------------------------------------------------------------

interface IdentToken {
  start: number;
  end: number;
  name: string;
}

const RESERVED = new Set(
  (
    "all analyse analyze and any array as asc asymmetric both case cast check collate column constraint create " +
    "current_catalog current_date current_role current_time current_timestamp current_user default deferrable desc " +
    "distinct do else end except false fetch for foreign from grant group having in initially intersect into lateral " +
    "leading limit localtime localtimestamp not null offset on only or order placing primary references returning " +
    "select session_user some symmetric system_user table then to trailing true union unique user using variadic when " +
    "where window with"
  ).split(" "),
);

function quoteIdent(name: string): string {
  return /^[a-z_][a-z0-9_$]*$/.test(name) && !RESERVED.has(name) ? name : `"${name.replace(/"/g, '""')}"`;
}

const IDENT_START = /[A-Za-z_\u0080-￿]/;
const IDENT_PART = /[A-Za-z0-9_$\u0080-￿]/;

/**
 * Column-like identifiers of an expression: bare (folded to lower case) or double-quoted, skipping string
 * literals, qualified names (`a.b`), function names (`f(`) and cast types (`::t`).
 */
function identTokens(sql: string): IdentToken[] {
  const raw: IdentToken[] = [];
  let i = 0;
  while (i < sql.length) {
    const ch = sql[i];
    if (ch === "'") {
      i++;
      while (i < sql.length) {
        if (sql[i] === "'") {
          if (sql[i + 1] === "'") {
            i += 2;
            continue;
          }
          i++;
          break;
        }
        i++;
      }
    } else if (ch === '"') {
      let j = i + 1;
      let name = "";
      while (j < sql.length) {
        if (sql[j] === '"') {
          if (sql[j + 1] === '"') {
            name += '"';
            j += 2;
            continue;
          }
          break;
        }
        name += sql[j];
        j++;
      }
      raw.push({ start: i, end: j + 1, name });
      i = j + 1;
    } else if (ch === "-" && sql[i + 1] === "-") {
      while (i < sql.length && sql[i] !== "\n") i++;
    } else if (/[0-9]/.test(ch)) {
      while (i < sql.length && IDENT_PART.test(sql[i])) i++;
    } else if (IDENT_START.test(ch)) {
      let j = i + 1;
      while (j < sql.length && IDENT_PART.test(sql[j])) j++;
      raw.push({ start: i, end: j, name: sql.slice(i, j).toLowerCase() });
      i = j;
    } else {
      i++;
    }
  }
  return raw.filter((t) => {
    let p = t.start - 1;
    while (p >= 0 && /\s/.test(sql[p])) p--;
    if (p >= 0 && sql[p] === ".") return false;
    if (p >= 1 && sql[p] === ":" && sql[p - 1] === ":") return false;
    let n = t.end;
    while (n < sql.length && /\s/.test(sql[n])) n++;
    return !(n < sql.length && (sql[n] === "(" || sql[n] === "."));
  });
}

function renameInExpression(sql: string, from: string, to: string): string {
  let out = "";
  let last = 0;
  for (const t of identTokens(sql)) {
    if (t.name !== from) continue;
    out += sql.slice(last, t.start) + quoteIdent(to);
    last = t.end;
  }
  return out + sql.slice(last);
}

// ---------------------------------------------------------------------------------------------
// Interpreter: the table as a sequence of operations leaves it
// ---------------------------------------------------------------------------------------------

interface SimCol {
  id: string;
  name: string;
  base: Column | null;
  type: TypeSpec | null; // added: its type; base: the pending type
  pending: boolean; // a base column with a pending type, collation or identity change
  not_null: boolean;
  default: string | null;
  comment: string | null;
  dropped: boolean;
}

interface SimCons {
  key: string;
  explicitName: string | null; // base, given or generated-when-added name; null = generated from current names
  baseName: string | null;
  tableAt: string; // the table's name when it was added (generated names use it)
  kind: ConstraintKindName;
  cols: string[]; // column ids
  target: FkTarget | null;
  definition: string | null;
  validated: boolean;
  baseValidated: boolean;
  added: boolean;
  dropped: boolean;
}

interface OpInfo {
  target: string | null; // the column an alter/rename/drop_column names
  refs: string[]; // every column it names
  defines: string[]; // columns it creates
  cons: string | null; // the constraint it adds or names
}

interface Sim {
  schema: string;
  name: string;
  comment: string | null;
  rls_enabled: boolean;
  rls_forced: boolean;
  dropped: boolean;
  baseRls: { enabled: boolean; forced: boolean } | null;
  baseComment: string | null;
  cols: SimCol[];
  cons: SimCons[];
  info: OpInfo[];
  invalid: number;
}

function liveCol(sim: Sim, name: string): SimCol | undefined {
  return sim.cols.find((c) => !c.dropped && c.name === name);
}

function colName(sim: Sim, id: string): string {
  return sim.cols.find((c) => c.id === id)?.name ?? id;
}

/** A foreign key's target: on this table, by column ids (they follow renames); elsewhere, by names. */
type FkTarget = { self: true; ids: string[] } | { self: false; schema: string; table: string; columns: string[] };

/** The parts Postgres builds a constraint's generated name from (`invoices`, `code`, `key`). */
interface NameParts {
  name1: string;
  name2: string | null;
  label: string;
}

function generatedName(table: string, kind: ConstraintKindName, cols: string[]): NameParts {
  switch (kind) {
    case "primary_key":
      return { name1: table, name2: null, label: "pkey" };
    case "unique":
      return { name1: table, name2: clipBytes(cols.join("_"), NAME_BYTES), label: "key" };
    case "foreign_key":
      return { name1: table, name2: clipBytes(cols.join("_"), NAME_BYTES), label: "fkey" };
    case "check":
      return { name1: table, name2: cols.length === 1 ? cols[0] : null, label: "check" };
    case "exclusion":
      return { name1: table, name2: clipBytes(cols.join("_"), NAME_BYTES), label: "excl" };
  }
}

/** NAMEDATALEN - 1: the longest identifier Postgres keeps, in bytes. */
const NAME_BYTES = 63;
const utf8 = new TextEncoder();

function byteLength(s: string): number {
  return utf8.encode(s).length;
}

/** The longest prefix of `s` that fits in `n` bytes, cut on a character boundary (`pg_mbcliplen`). */
function clipBytes(s: string, n: number): string {
  let out = "";
  let used = 0;
  for (const ch of s) {
    const l = byteLength(ch);
    if (used + l > n) break;
    out += ch;
    used += l;
  }
  return out;
}

/** Postgres's `makeObjectName`: `name1_name2_label` within 63 bytes, shortening the longer of name1 and name2 first. */
function makeObjectName({ name1, name2, label }: NameParts): string {
  let overhead = 0;
  let n1 = byteLength(name1);
  let n2 = 0;
  if (name2 !== null) {
    n2 = byteLength(name2);
    overhead++;
  }
  if (label) overhead += byteLength(label) + 1;
  const avail = NAME_BYTES - overhead;
  while (n1 + n2 > avail) {
    if (n1 > n2) n1--;
    else n2--;
  }
  let out = clipBytes(name1, n1);
  if (name2 !== null) out += `_${clipBytes(name2, n2)}`;
  if (label) out += `_${label}`;
  return out;
}

/** Postgres's `ChooseConstraintName`: a taken name gets a number inside the label (`key1`, `key2`, …), truncated again. */
function freeName(parts: NameParts, taken: Set<string>): string {
  const first = makeObjectName(parts);
  if (!taken.has(first)) return first;
  for (let n = 1; ; n++) {
    const name = makeObjectName({ ...parts, label: `${parts.label}${n}` });
    if (!taken.has(name)) return name;
  }
}

/**
 * Every constraint's current name. Fixed names (base, given, or generated when added) come first; a
 * constraint on a column added in this changeset is named from the column's current name, since fold
 * rewrites its add under that name.
 */
function consNames(sim: Sim): Map<SimCons, string> {
  const names = new Map<SimCons, string>();
  const taken = new Set<string>();
  for (const c of sim.cons) {
    if (c.explicitName !== null) {
      names.set(c, c.explicitName);
      if (!c.dropped) taken.add(c.explicitName);
    }
  }
  for (const c of sim.cons) {
    if (c.explicitName !== null) continue;
    const name = freeName(generatedName(c.tableAt, c.kind, c.cols.map((id) => colName(sim, id))), taken);
    names.set(c, name);
    if (!c.dropped) taken.add(name);
  }
  return names;
}

function liveCons(sim: Sim, name: string): SimCons | undefined {
  const names = consNames(sim);
  return sim.cons.find((c) => !c.dropped && names.get(c) === name);
}

/** `REFERENCES t(a, "B")` in a `pg_get_constraintdef` text → `["a", "B"]`. */
function referencedNames(definition: string): string[] {
  const m = definition.match(/REFERENCES\s+(?:"(?:[^"]|"")*"|[^\s(]+)\s*\(([^)]*)\)/i);
  if (!m) return [];
  return m[1].split(",").map((s) => {
    const t = s.trim();
    return t.startsWith('"') && t.endsWith('"') ? t.slice(1, -1).replace(/""/g, '"') : t;
  });
}

function baseTarget(base: Table, c: Table["constraints"][number]): FkTarget | null {
  if (c.kind !== "foreign_key" || !c.references) return null;
  const r = c.references;
  if (r.table_oid === base.oid) return { self: true, ids: r.columns.map((n) => `b:${n}`) };
  return { self: false, schema: r.schema, table: r.table, columns: referencedNames(c.definition) };
}

function targetIds(c: SimCons): string[] {
  return c.target?.self ? c.target.ids : [];
}

function specColumns(spec: ConstraintSpec): string[] {
  const k = spec.kind;
  if ("primary_key" in k) return k.primary_key.columns;
  if ("unique" in k) return k.unique.columns;
  if ("foreign_key" in k) return k.foreign_key.columns;
  if ("exclusion" in k) return k.exclusion.elements.map((e) => e.column);
  return [];
}

function addColumnTo(sim: Sim, spec: ColumnSpec, id: string): void {
  sim.cols.push({
    id,
    name: spec.name,
    base: null,
    type: spec.data_type,
    pending: false,
    not_null: spec.not_null ?? false,
    default: spec.default ?? null,
    comment: spec.comment ?? null,
    dropped: false,
  });
}

/** Adds a constraint; returns the column ids it names, or null when one of them doesn't exist. */
function addConstraintTo(sim: Sim, spec: ConstraintSpec, key: string, info: OpInfo): boolean {
  const kind = specKind(spec);
  let ids: string[];
  if ("check" in spec.kind) {
    const names = new Set(identTokens(spec.kind.check.expression).map((t) => t.name));
    ids = sim.cols.filter((c) => !c.dropped && names.has(c.name)).map((c) => c.id);
  } else {
    const found = specColumns(spec).map((n) => liveCol(sim, n));
    if (found.some((c) => c === undefined)) return false;
    ids = found.map((c) => c!.id);
  }
  let target: FkTarget | null = null;
  if ("foreign_key" in spec.kind) {
    const fk = spec.kind.foreign_key;
    if (fk.references.schema === sim.schema && fk.references.name === sim.name) {
      const found = fk.referenced_columns.map((n) => liveCol(sim, n));
      if (found.some((c) => c === undefined)) return false;
      target = { self: true, ids: found.map((c) => c!.id) };
    } else target = { self: false, schema: fk.references.schema, table: fk.references.name, columns: fk.referenced_columns };
  }
  const refs = [...ids, ...(target?.self ? target.ids : [])];
  let name = spec.name ?? null;
  if (name !== null && liveCons(sim, name)) return false;
  if (name === null && refs.every((id) => id.startsWith("b:"))) {
    // Postgres fixes a generated name when the constraint is added; later renames don't change it.
    const names = consNames(sim);
    const taken = new Set(sim.cons.filter((c) => !c.dropped).map((c) => names.get(c)!));
    name = freeName(generatedName(sim.name, kind, ids.map((id) => colName(sim, id))), taken);
  }
  info.refs.push(...refs);
  info.cons = key;
  sim.cons.push({
    key,
    explicitName: name,
    baseName: null,
    tableAt: sim.name,
    kind,
    cols: ids,
    target,
    definition: null,
    validated: !(spec.not_valid ?? false),
    baseValidated: true,
    added: true,
    dropped: false,
  });
  return true;
}

function applyChange(col: SimCol, change: ColumnChange): void {
  if (change.data_type) {
    col.type = change.data_type;
    if (col.base) col.pending = true;
  }
  if (col.base && (change.collation || change.identity)) col.pending = true;
  if (change.not_null !== undefined && change.not_null !== null) col.not_null = change.not_null;
  if (change.default) col.default = change.default === "drop" ? null : change.default.set;
  if (change.comment) col.comment = change.comment === "clear" ? null : change.comment.set;
}

function dropColumnFrom(sim: Sim, col: SimCol): void {
  // Postgres drops the column's constraints with it.
  for (const c of sim.cons) {
    if (c.dropped || !(c.cols.includes(col.id) || targetIds(c).includes(col.id))) continue;
    if (c.added) c.dropped = true;
    else {
      c.dropped = true;
      c.explicitName = c.baseName;
      c.validated = c.baseValidated;
    }
  }
  sim.cons = sim.cons.filter((c) => !(c.added && c.dropped));
  if (!col.base) {
    sim.cols = sim.cols.filter((c) => c !== col);
    return;
  }
  const b = col.base;
  Object.assign(col, {
    name: b.name,
    type: null,
    pending: false,
    not_null: b.not_null,
    default: b.default ?? null,
    comment: b.comment ?? null,
    dropped: true,
  });
}

function simulate(base: Table | null, ops: Operation[]): Sim {
  const sim: Sim = {
    schema: base?.schema ?? "",
    name: base?.name ?? "",
    comment: base?.comment ?? null,
    rls_enabled: base?.rls_enabled ?? false,
    rls_forced: base?.rls_forced ?? false,
    dropped: false,
    baseRls: base ? { enabled: base.rls_enabled, forced: base.rls_forced } : null,
    baseComment: base?.comment ?? null,
    cols: [],
    cons: [],
    info: [],
    invalid: 0,
  };
  if (base) {
    for (const c of base.columns) {
      sim.cols.push({
        id: `b:${c.attnum}`,
        name: c.name,
        base: c,
        type: null,
        pending: false,
        not_null: c.not_null,
        default: c.default ?? null,
        comment: c.comment ?? null,
        dropped: false,
      });
    }
    for (const c of base.constraints) {
      sim.cons.push({
        key: `b:${c.name}`,
        explicitName: c.name,
        baseName: c.name,
        tableAt: base.name,
        kind: c.kind,
        cols: c.columns.map((n) => `b:${n}`).filter((id) => sim.cols.some((x) => x.id === id)),
        target: baseTarget(base, c),
        definition: c.definition,
        validated: c.validated,
        baseValidated: c.validated,
        added: false,
        dropped: false,
      });
    }
  }
  let created = base !== null;
  ops.forEach((op, i) => {
    const info: OpInfo = { target: null, refs: [], defines: [], cons: null };
    sim.info.push(info);
    let ok = true;
    if ("create_table" in op) {
      const ct = op.create_table;
      if (created) ok = false;
      else {
        created = true;
        sim.schema = ct.table.schema;
        sim.name = ct.table.name;
        sim.comment = ct.comment ?? null;
        sim.rls_enabled = ct.rls?.enabled ?? false;
        sim.rls_forced = ct.rls?.forced ?? false;
        ct.columns.forEach((spec, k) => {
          const id = `a:${i}:${k}`;
          addColumnTo(sim, spec, id);
          info.defines.push(id);
        });
        if (ct.primary_key && ct.primary_key.length > 0) {
          ok = addConstraintTo(sim, { kind: { primary_key: { columns: ct.primary_key } } }, `p:${i}:pk`, info) && ok;
        }
        (ct.constraints ?? []).forEach((spec, k) => {
          ok = addConstraintTo(sim, spec, `p:${i}:${k}`, info) && ok;
        });
        info.cons = null;
      }
    } else if ("add_column" in op) {
      const spec = op.add_column.column;
      if (liveCol(sim, spec.name)) ok = false;
      else {
        const id = `a:${i}`;
        addColumnTo(sim, spec, id);
        info.defines.push(id);
      }
    } else if ("alter_column" in op || "rename_column" in op || "drop_column" in op) {
      const name = "alter_column" in op ? op.alter_column.column : "rename_column" in op ? op.rename_column.column : op.drop_column.column;
      const col = liveCol(sim, name);
      if (!col) ok = false;
      else {
        info.target = col.id;
        info.refs.push(col.id);
        if ("alter_column" in op) applyChange(col, op.alter_column.change);
        else if ("rename_column" in op) {
          const to = op.rename_column.to;
          if (to !== col.name && liveCol(sim, to)) ok = false;
          else col.name = to;
        } else dropColumnFrom(sim, col);
      }
    } else if ("add_constraint" in op) {
      ok = addConstraintTo(sim, op.add_constraint.constraint, `p:${i}`, info);
    } else if ("drop_constraint" in op || "rename_constraint" in op || "validate_constraint" in op) {
      const name = "drop_constraint" in op ? op.drop_constraint.name : "rename_constraint" in op ? op.rename_constraint.name : op.validate_constraint.name;
      const c = liveCons(sim, name);
      if (!c) ok = false;
      else {
        info.cons = c.key;
        info.refs.push(...c.cols, ...targetIds(c));
        if ("drop_constraint" in op) {
          c.dropped = true;
          if (c.added) sim.cons = sim.cons.filter((x) => x !== c);
          else {
            c.explicitName = c.baseName;
            c.validated = c.baseValidated;
          }
        } else if ("rename_constraint" in op) {
          if (liveCons(sim, op.rename_constraint.to)) ok = false;
          else c.explicitName = op.rename_constraint.to;
        } else c.validated = true;
      }
    } else if ("create_index" in op) {
      for (const k of op.create_index.keys ?? []) {
        const c = "column" in k ? liveCol(sim, k.column.name) : undefined;
        if (c) info.refs.push(c.id);
      }
      for (const n of op.create_index.include ?? []) {
        const c = liveCol(sim, n);
        if (c) info.refs.push(c.id);
      }
    } else if ("set_rls" in op) {
      sim.rls_enabled = op.set_rls.enabled;
      sim.rls_forced = op.set_rls.forced;
    } else if ("rename_table" in op) {
      sim.name = op.rename_table.to;
    } else if ("set_table_schema" in op) {
      sim.schema = op.set_table_schema.schema;
    } else if ("set_table_comment" in op) {
      sim.comment = op.set_table_comment.comment ?? null;
    } else if ("drop_table" in op) {
      sim.dropped = true;
    }
    if (!ok) sim.invalid++;
  });
  return sim;
}

function sameTypeAsBase(col: SimCol): boolean {
  return !col.pending;
}

function projectSim(sim: Sim): ProjectedTable {
  const columns: ProjectedColumn[] = sim.cols.map((c) => {
    const b = c.base;
    const type_spec = b === null || c.pending ? c.type : null;
    let mark: Mark | null = null;
    if (b === null) mark = "added";
    else if (c.dropped) mark = "dropped";
    else if (
      c.name !== b.name ||
      !sameTypeAsBase(c) ||
      c.not_null !== b.not_null ||
      c.default !== (b.default ?? null) ||
      c.comment !== (b.comment ?? null)
    )
      mark = "changed";
    return {
      name: c.name,
      original: b?.name ?? null,
      type_oid: b !== null && !c.pending ? b.type_oid : null,
      type_spec,
      data_type: type_spec ? typeSpecText(type_spec) : (b?.data_type ?? ""),
      not_null: c.not_null,
      default: c.default,
      comment: c.comment,
      mark,
    };
  });
  const names = consNames(sim);
  const constraints: ProjectedConstraint[] = sim.cons.map((c) => {
    const name = names.get(c)!;
    let mark: Mark | null = null;
    if (c.added) mark = "added";
    else if (c.dropped) mark = "dropped";
    else if (name !== c.baseName || c.validated !== c.baseValidated) mark = "changed";
    const t = c.target;
    const references = t === null ? null : t.self ? { schema: sim.schema, table: sim.name, columns: t.ids.map((id) => colName(sim, id)) } : { schema: t.schema, table: t.table, columns: t.columns };
    return { name, kind: c.kind, columns: c.cols.map((id) => colName(sim, id)), definition: c.definition, validated: c.validated, mark, references };
  });
  return { schema: sim.schema, name: sim.name, comment: sim.comment, rls_enabled: sim.rls_enabled, dropped: sim.dropped, columns, constraints };
}

/** The table as it will look once `edits` apply; with no base, the first `create_table` gives the table. */
export function project(base: Table | null, edits: Edit[]): ProjectedTable {
  return projectSim(simulate(base, opsOf(edits)));
}

// ---------------------------------------------------------------------------------------------
// Fold: keep the changeset minimal
// ---------------------------------------------------------------------------------------------

interface WEdit {
  id: string;
  label: string;
  ops: Operation[];
  dirty: boolean;
  work: boolean; // the edit being folded in
}

interface Pos {
  e: number;
  o: number;
}

function flatOps(w: WEdit[]): Operation[] {
  return w.flatMap((e) => e.ops);
}

function positions(w: WEdit[]): Pos[] {
  const out: Pos[] = [];
  w.forEach((e, ei) => e.ops.forEach((_, oi) => out.push({ e: ei, o: oi })));
  return out;
}

function cloneW(w: WEdit[]): WEdit[] {
  return w.map((e) => ({ ...e, ops: clone(e.ops) }));
}

/** Removes the ops at the given flat indexes (marking their edits rewritten). */
function removeAt(w: WEdit[], flat: number[]): void {
  const pos = positions(w);
  const byEdit = new Map<number, Set<number>>();
  for (const f of flat) {
    const p = pos[f];
    if (!byEdit.has(p.e)) byEdit.set(p.e, new Set());
    byEdit.get(p.e)!.add(p.o);
  }
  for (const [e, set] of byEdit) {
    w[e].ops = w[e].ops.filter((_, i) => !set.has(i));
    w[e].dirty = true;
  }
}

function opAt(w: WEdit[], f: number): Operation {
  const p = positions(w)[f];
  return w[p.e].ops[p.o];
}

function touch(w: WEdit[], f: number): void {
  w[positions(w)[f].e].dirty = true;
}

/** Appends `op` at the very end: to the edit being folded in when it is last, else to a fresh last edit. */
function appendOp(w: WEdit[], op: Operation): void {
  const last = w[w.length - 1];
  if (last && last.work) {
    last.ops.push(op);
    return;
  }
  for (const e of w) e.work = false;
  w.push({ id: crypto.randomUUID(), label: "", ops: [op], dirty: true, work: true });
}

function renameColumnsIn(spec: ConstraintSpec, from: string, to: string, selfRef: boolean): void {
  const map = (cols: string[]) => cols.map((c) => (c === from ? to : c));
  const k = spec.kind;
  if ("primary_key" in k) k.primary_key.columns = map(k.primary_key.columns);
  else if ("unique" in k) k.unique.columns = map(k.unique.columns);
  else if ("foreign_key" in k) {
    k.foreign_key.columns = map(k.foreign_key.columns);
    if (selfRef) k.foreign_key.referenced_columns = map(k.foreign_key.referenced_columns);
  } else if ("exclusion" in k) k.exclusion.elements = k.exclusion.elements.map((el) => (el.column === from ? { ...el, column: to } : el));
  else if ("check" in k) k.check.expression = renameInExpression(k.check.expression, from, to);
}

function isSelfRef(spec: ConstraintSpec, table: TableRef): boolean {
  return "foreign_key" in spec.kind && spec.kind.foreign_key.references.schema === table.schema && spec.kind.foreign_key.references.name === table.name;
}

/** Renames column `from` to `to` wherever `op` names it. */
function renameColumnIn(op: Operation, from: string, to: string): void {
  if ("alter_column" in op && op.alter_column.column === from) op.alter_column.column = to;
  else if ("rename_column" in op && op.rename_column.column === from) op.rename_column.column = to;
  else if ("drop_column" in op && op.drop_column.column === from) op.drop_column.column = to;
  else if ("add_constraint" in op) renameColumnsIn(op.add_constraint.constraint, from, to, isSelfRef(op.add_constraint.constraint, op.add_constraint.table));
  else if ("create_index" in op) {
    const ix = op.create_index;
    ix.keys = ix.keys?.map((k) => ("column" in k && k.column.name === from ? { column: { ...k.column, name: to } } : k));
    ix.include = ix.include?.map((c) => (c === from ? to : c));
  } else if ("create_table" in op) {
    const ct = op.create_table;
    ct.primary_key = ct.primary_key?.map((c) => (c === from ? to : c));
    for (const spec of ct.constraints ?? []) renameColumnsIn(spec, from, to, isSelfRef(spec, ct.table));
  }
}

/** Where a column added in this changeset is defined: its `add_column`, or its entry in a `create_table`. */
function defineSite(sim: Sim, id: string): number {
  return sim.info.findIndex((x) => x.defines.includes(id));
}

function siteSpec(w: WEdit[], f: number, id: string): ColumnSpec {
  const op = opAt(w, f);
  if ("add_column" in op) return op.add_column.column;
  if ("create_table" in op) return op.create_table.columns[Number(id.split(":")[2])];
  throw new Error("not a column definition");
}

function changeIntoSpec(spec: ColumnSpec, ch: ColumnChange): void {
  if (ch.data_type) spec.data_type = ch.data_type;
  if (ch.collation) spec.collation = ch.collation;
  if (ch.not_null !== undefined && ch.not_null !== null) {
    if (ch.not_null) spec.not_null = true;
    else delete spec.not_null;
  }
  if (ch.default) {
    if (ch.default === "drop") delete spec.default;
    else spec.default = ch.default.set;
  }
  if (ch.comment) {
    if (ch.comment === "clear") delete spec.comment;
    else spec.comment = ch.comment.set;
  }
  if (ch.identity) {
    if (ch.identity === "drop") delete spec.identity;
    else spec.identity = "add" in ch.identity ? ch.identity.add : ch.identity.set;
  }
}

function mergeChange(into: ColumnChange, ch: ColumnChange): void {
  for (const [k, v] of Object.entries(ch)) {
    if (v !== undefined && v !== null) (into as Record<string, unknown>)[k] = v;
  }
}

/** Drops the fields of a change that leave the base column as it is; an empty change means no change. */
function stripChange(ch: ColumnChange, b: Column): ColumnChange {
  const out: ColumnChange = {};
  for (const [k, v] of Object.entries(ch)) {
    if (v !== undefined && v !== null) (out as Record<string, unknown>)[k] = v;
  }
  if (out.not_null !== undefined && out.not_null === b.not_null) delete out.not_null;
  if (out.default && (out.default === "drop" ? null : out.default.set) === (b.default ?? null)) delete out.default;
  if (out.comment && (out.comment === "clear" ? null : out.comment.set) === (b.comment ?? null)) delete out.comment;
  return out;
}

function isEmpty(o: object): boolean {
  return Object.keys(o).length === 0;
}

/** The F-rules: the minimal rewrite of `w` that has the effect of appending `op`; null when none applies. */
function foldCandidate(sim: Sim, w0: WEdit[], op: Operation): WEdit[][] {
  const w = cloneW(w0);
  if ("add_column" in op) {
    const ct = sim.info.findIndex((_, f) => "create_table" in opAt(w, f));
    if (ct === -1) return [];
    const target = opAt(w, ct);
    if (!("create_table" in target)) return [];
    target.create_table.columns.push(op.add_column.column);
    touch(w, ct);
    return [w];
  }

  if ("alter_column" in op || "rename_column" in op || "drop_column" in op) {
    const name = "alter_column" in op ? op.alter_column.column : "rename_column" in op ? op.rename_column.column : op.drop_column.column;
    const col = liveCol(sim, name);
    if (!col) return [];
    const id = col.id;
    const refsAfter = (from: number) => sim.info.map((x, f) => (f > from && x.refs.includes(id) ? f : -1)).filter((f) => f !== -1);

    if (col.base === null) {
      // F1, F2, F6: rewrite the column's definition.
      const site = defineSite(sim, id);
      if (site === -1) return [];
      const spec = siteSpec(w, site, id);
      if ("alter_column" in op) {
        changeIntoSpec(spec, op.alter_column.change);
        touch(w, site);
      } else if ("rename_column" in op) {
        const to = op.rename_column.to;
        if (to === name) return [w];
        spec.name = to;
        renameColumnIn(opAt(w, site), name, to);
        touch(w, site);
        for (const f of refsAfter(site)) {
          renameColumnIn(opAt(w, f), name, to);
          touch(w, f);
        }
      } else {
        const def = opAt(w, site);
        const drop = refsAfter(site);
        if ("create_table" in def) {
          const ct = def.create_table;
          ct.columns = ct.columns.filter((c) => c !== spec);
          if (ct.primary_key) {
            ct.primary_key = ct.primary_key.filter((c) => c !== name);
            if (ct.primary_key.length === 0) delete ct.primary_key;
          }
          if (ct.constraints) {
            ct.constraints = ct.constraints.filter((s) => {
              const names = "check" in s.kind ? identTokens(s.kind.check.expression).map((t) => t.name) : specColumns(s);
              const referenced = "foreign_key" in s.kind && isSelfRef(s, ct.table) ? s.kind.foreign_key.referenced_columns : [];
              return !names.includes(name) && !referenced.includes(name);
            });
          }
          touch(w, site);
        } else drop.push(site);
        removeAt(w, drop);
      }
      return [w];
    }

    const b = col.base;
    const renames = sim.info.map((x, f) => (x.target === id && "rename_column" in opAt(w, f) ? f : -1)).filter((f) => f !== -1);

    if ("alter_column" in op) {
      // F3: one alter per base column; later alters merge into it, wherever it is.
      const alters = sim.info.map((x, f) => (x.target === id && "alter_column" in opAt(w, f) ? f : -1)).filter((f) => f !== -1);
      if (alters.length > 0) {
        const f = alters[0];
        const existing = opAt(w, f);
        if (!("alter_column" in existing)) return [];
        mergeChange(existing.alter_column.change, op.alter_column.change);
        const change = stripChange(existing.alter_column.change, b);
        if (isEmpty(change)) removeAt(w, [f]);
        else {
          existing.alter_column.change = change;
          touch(w, f);
        }
        return [w];
      }
      const change = stripChange(op.alter_column.change, b);
      if (isEmpty(change)) return [w];
      // M1: the alter stays in the user's own edit, under the column's current name.
      const appended = cloneW(w0);
      appendOp(appended, { alter_column: { table: op.alter_column.table, column: name, change } });
      if (renames.length === 0) return [appended];
      // Only when that name was another column's alter target earlier (a name reused after a rename):
      // the alter goes before the column's first rename, under its base name, so alters stay one per name.
      const r = renames[0];
      const rOp = opAt(w, r);
      if (!("rename_column" in rOp)) return [appended];
      const alter: Operation = { alter_column: { table: op.alter_column.table, column: rOp.rename_column.column, change } };
      const p = positions(w)[r];
      if (w[p.e].work) w[p.e].ops.splice(p.o, 0, alter);
      else w.splice(p.e, 0, { id: crypto.randomUUID(), label: "", ops: [alter], dirty: true, work: false });
      return [appended, w];
    }

    if ("rename_column" in op) {
      // F4: renames chain into one (the earliest that stays valid); back to its old name cancels it.
      const to = op.rename_column.to;
      if (to === name) return [w];
      const nameAt = (f: number) => {
        const r = renames.filter((x) => x < f).pop();
        if (r === undefined) return b.name;
        const rOp = opAt(w0, r);
        return "rename_column" in rOp ? rOp.rename_column.to : b.name;
      };
      return renames.map((r) => {
        const c = cloneW(w0);
        const rOp = opAt(c, r);
        if (!("rename_column" in rOp)) return c;
        const before = rOp.rename_column.column;
        const final = to === before ? before : to;
        for (const f of refsAfter(r)) {
          if (renames.includes(f)) continue;
          renameColumnIn(opAt(c, f), nameAt(f), final);
          touch(c, f);
        }
        const later = renames.filter((x) => x > r);
        if (to === before) removeAt(c, [r, ...later]);
        else {
          rOp.rename_column.to = to;
          touch(c, r);
          removeAt(c, later);
        }
        return c;
      });
    }

    // F5: a dropped base column loses its pending changes and is dropped by its base name.
    const related = sim.info.map((x, f) => (x.refs.includes(id) ? f : -1)).filter((f) => f !== -1);
    if (renames.length > 0) {
      const r = renames[0];
      const rOp = opAt(w, r);
      if (!("rename_column" in rOp)) return [];
      const p = positions(w)[r];
      w[p.e].ops[p.o] = { drop_column: { table: op.drop_column.table, column: rOp.rename_column.column } };
      w[p.e].dirty = true;
      removeAt(
        w,
        related.filter((f) => f !== r),
      );
      return [w];
    }
    if (related.length === 0) return [];
    removeAt(w, related);
    appendOp(w, op);
    return [w];
  }

  if ("drop_constraint" in op) {
    const c = liveCons(sim, op.drop_constraint.name);
    if (!c) return [];
    const naming = sim.info.map((x, f) => (x.cons === c.key ? f : -1)).filter((f) => f !== -1);
    if (c.added) {
      // A pending constraint dropped again: remove it and what names it.
      const def = c.key.split(":");
      if (def.length === 3) {
        const ct = opAt(w, Number(def[1]));
        if (!("create_table" in ct)) return [];
        if (def[2] === "pk") delete ct.create_table.primary_key;
        else ct.create_table.constraints = (ct.create_table.constraints ?? []).filter((_, k) => k !== Number(def[2]));
        touch(w, Number(def[1]));
        removeAt(w, naming);
      } else removeAt(w, [...naming, Number(def[1])]);
      return [w];
    }
    if (naming.length === 0) return [];
    removeAt(w, naming);
    appendOp(w, { drop_constraint: { table: op.drop_constraint.table, name: c.baseName ?? op.drop_constraint.name } });
    return [w];
  }

  if ("set_rls" in op || "set_table_comment" in op) {
    // Table-level settings: one pending op each, gone when back to the base.
    const key = opKey(op);
    const existing = sim.info.map((_, f) => (opKey(opAt(w, f)) === key ? f : -1)).filter((f) => f !== -1);
    const backToBase =
      "set_rls" in op
        ? sim.baseRls !== null && op.set_rls.enabled === sim.baseRls.enabled && op.set_rls.forced === sim.baseRls.forced
        : sim.baseRls !== null && (op.set_table_comment.comment ?? null) === sim.baseComment;
    if (existing.length === 0) return backToBase ? [w] : [];
    // A table rename or move after the pending op: the new value stays after it (it names the new table).
    const renamedSince = sim.info.some((_, f) => f > existing[0] && ("rename_table" in opAt(w, f) || "set_table_schema" in opAt(w, f)));
    if (backToBase) removeAt(w, existing);
    else if (renamedSince) {
      removeAt(w, existing);
      appendOp(w, clone(op));
    } else {
      const p = positions(w)[existing[0]];
      w[p.e].ops[p.o] = clone(op);
      w[p.e].dirty = true;
      removeAt(w, existing.slice(1));
    }
    return [w];
  }

  return [];
}

function reordered(w: WEdit[], order: number[]): WEdit[] {
  const next = order.map((k) => ({ ...w[k] }));
  // The edit being folded in must stay last for later appends; elsewhere it is an ordinary edit.
  next.forEach((e, k) => (e.work = e.work && k === next.length - 1));
  return next;
}

/**
 * Edit orders to try when a fold would rename a column into a name another column still holds at
 * that point: edits that don't touch added columns move earlier (they free the name). Gentlest first: one
 * edit moved, then column-only edits of a span moved, then every unrelated edit of a span moved.
 */
function* reorderings(base: Table | null, w: WEdit[]): Generator<WEdit[]> {
  const sim = simulate(base, flatOps(w));
  const pos = positions(w);
  const ops = flatOps(w);
  const related = new Set<number>();
  const columnOnly = new Set(w.map((_, e) => e));
  sim.info.forEach((x, f) => {
    const op = ops[f];
    if ("add_column" in op || "create_table" in op || x.refs.some((id) => id.startsWith("a:"))) related.add(pos[f].e);
    if (!("alter_column" in op || "rename_column" in op || "drop_column" in op)) columnOnly.delete(pos[f].e);
  });
  const idx = w.map((_, k) => k);
  for (let i = 0; i < w.length; i++) {
    for (let j = i + 1; j < w.length; j++) {
      if (related.has(j)) continue;
      yield reordered(w, [...idx.slice(0, i), j, ...idx.slice(i, j), ...idx.slice(j + 1)]);
    }
  }
  for (const movable of [(k: number) => columnOnly.has(k) && !related.has(k), (k: number) => !related.has(k)]) {
    for (let len = 2; len <= w.length; len++) {
      for (let i = 0; i + len <= w.length; i++) {
        const seg = idx.slice(i, i + len);
        const first = seg.filter(movable);
        const rest = seg.filter((k) => !movable(k));
        const order = [...first, ...rest];
        if (first.length === 0 || rest.length === 0 || order.every((k, n) => k === seg[n])) continue;
        yield reordered(w, [...idx.slice(0, i), ...order, ...idx.slice(i + len)]);
      }
    }
  }
}

/** No column op names an added column, and no column name is altered or renamed by two ops. */
function tidy(w: WEdit[]): boolean {
  const added = new Set<string>();
  const altered = new Set<string>();
  const renamed = new Set<string>();
  for (const op of flatOps(w)) {
    if ("add_column" in op) added.add(op.add_column.column.name);
    const c = "alter_column" in op ? op.alter_column.column : "rename_column" in op ? op.rename_column.column : "drop_column" in op ? op.drop_column.column : null;
    if (c === null) continue;
    if (added.has(c)) return false;
    const seen = "alter_column" in op ? altered : "rename_column" in op ? renamed : null;
    if (seen?.has(c)) return false;
    seen?.add(c);
  }
  return true;
}

function foldOp(base: Table | null, w: WEdit[], op: Operation): WEdit[] {
  const naive = cloneW(w);
  appendOp(naive, clone(op));
  const sim = simulate(base, flatOps(w));
  const naiveSim = simulate(base, flatOps(naive));
  if (naiveSim.invalid > sim.invalid) return naive; // the op itself doesn't apply: keep it as given
  const target = JSON.stringify(projectSim(naiveSim));
  const acceptable = (c: WEdit[]) => {
    const s = simulate(base, flatOps(c));
    return s.invalid <= sim.invalid && JSON.stringify(projectSim(s)) === target;
  };
  const candidates = foldCandidate(sim, w, op);
  for (const c of candidates) if (acceptable(c) && tidy(c)) return c;
  for (const c of candidates) if (acceptable(c)) return c;
  for (const c of candidates) for (const r of reorderings(base, c)) if (acceptable(r)) return r;
  return naive;
}

/** Folds `next` into `edits` (already folded): the result has the same effect with no redundant operation. */
export function fold(base: Table | null, edits: Edit[], next: Edit): Edit[] {
  // The new edit may name the table by its base name or its current one: address it first.
  const names = tableNames(base, [...opsOf(edits), ...next.ops]);
  const all = addressEdits(base, [...edits, next], names);
  edits = all.slice(0, -1);
  next = all[all.length - 1];
  // A fold can make an earlier operation foldable that wasn't when it was added (a name freed by a drop):
  // refold until nothing changes, so the result is a fixed point (folding it again changes nothing).
  let out = foldOnce(base, edits, next);
  for (let pass = 0; pass < 4; pass++) {
    const again = out.reduce<Edit[]>((acc, e) => foldOnce(base, acc, e), []);
    if (JSON.stringify(opsOf(again)) === JSON.stringify(opsOf(out))) break;
    out = again;
  }
  return addressEdits(base, out, names);
}

// ---------------------------------------------------------------------------------------------
// Addressing: every operation names the table as it is at that operation's position
// ---------------------------------------------------------------------------------------------

function refKey(t: TableRef): string {
  return `${t.schema}.${t.name}`;
}

/** The table an operation is scoped to (not a `create_table`'s, which defines it). */
function scopedTable(op: Operation): TableRef | null {
  if ("create_table" in op) return null;
  const body = Object.values(op)[0] as { table?: unknown } | undefined;
  const t = body?.table;
  return t !== null && typeof t === "object" && "schema" in t && "name" in t ? (t as TableRef) : null;
}

/** Every name (`schema.name`) the table has along `ops`: the base's, a `create_table`'s, and each rename's or move's result. */
function tableNames(base: Table | null, ops: Operation[]): Set<string> {
  const names = new Set<string>();
  let cur: TableRef | null = base ? { schema: base.schema, name: base.name } : null;
  if (cur) names.add(refKey(cur));
  for (const op of ops) {
    if ("create_table" in op) cur = { ...op.create_table.table };
    else if (cur && "rename_table" in op) cur = { ...cur, name: op.rename_table.to };
    else if (cur && "set_table_schema" in op) cur = { ...cur, schema: op.set_table_schema.schema };
    if (cur) names.add(refKey(cur));
  }
  return names;
}

/**
 * `ops` with each table-scoped operation naming the table as it is at its position (after the earlier
 * renames and moves), and a self-referencing foreign key (one that references any name the table has
 * had, in `ops` or in `alsoNames`) referencing it by that name too. Ops that are already addressed are
 * returned as they are.
 */
export function addressOps(base: Table | null, ops: Operation[], alsoNames: Iterable<string> = []): Operation[] {
  const names = tableNames(base, ops);
  for (const n of alsoNames) names.add(n);
  let cur: TableRef | null = base ? { schema: base.schema, name: base.name } : null;
  return ops.map((op) => {
    let out = op;
    if ("create_table" in op) cur = { ...op.create_table.table };
    else if (cur) {
      const t = scopedTable(op);
      const here = cur;
      const fk = "add_constraint" in op && "foreign_key" in op.add_constraint.constraint.kind ? op.add_constraint.constraint.kind.foreign_key : null;
      const fkOff = fk !== null && names.has(refKey(fk.references)) && refKey(fk.references) !== refKey(here);
      if (t && (refKey(t) !== refKey(here) || fkOff)) {
        out = clone(op);
        (Object.values(out)[0] as { table: TableRef }).table = { schema: here.schema, name: here.name };
        if (fkOff && "add_constraint" in out && "foreign_key" in out.add_constraint.constraint.kind) {
          out.add_constraint.constraint.kind.foreign_key.references = { schema: here.schema, name: here.name };
        }
      }
      if ("rename_table" in op) cur = { ...here, name: op.rename_table.to };
      else if ("set_table_schema" in op) cur = { ...here, schema: op.set_table_schema.schema };
    }
    return out;
  });
}

/** `edits` addressed (`addressOps`); an edit whose operations changed gets its label recomputed. */
function addressEdits(base: Table | null, edits: Edit[], alsoNames: Iterable<string> = []): Edit[] {
  const ops = addressOps(base, opsOf(edits), alsoNames);
  let i = 0;
  return edits.map((e) => {
    const next = ops.slice(i, i + e.ops.length);
    i += e.ops.length;
    if (next.every((o, k) => o === e.ops[k])) return e;
    return { ...e, ops: next, label: next.length > 0 ? editLabel(next[0]) : e.label };
  });
}

function foldOnce(base: Table | null, edits: Edit[], next: Edit): Edit[] {
  let w: WEdit[] = edits.map((e) => ({ id: e.id, label: e.label, ops: clone(e.ops), dirty: false, work: false }));
  w.push({ id: next.id, label: next.label, ops: [], dirty: false, work: true });
  for (const op of next.ops) w = foldOp(base, w, op);
  const work = w.find((e) => e.id === next.id);
  if (work && JSON.stringify(work.ops) !== JSON.stringify(next.ops)) work.dirty = true;
  return w
    .filter((e) => e.ops.length > 0)
    .map((e) => ({ id: e.id, ops: e.ops, label: e.dirty || e.label === "" ? editLabel(e.ops[0]) : e.label }));
}

export function foldAll(base: Table | null, edits: Edit[]): Edit[] {
  return edits.reduce<Edit[]>((acc, e) => fold(base, acc, e), []);
}

/**
 * `edits` without the edit `id`, and without every later edit that no longer applies once it is gone
 * (judged against the projection so far): a constraint on a column the removed edit added, an alter
 * on a column it renamed. An edit that didn't apply before the removal isn't dropped for it.
 */
export function withoutEdit(base: Table | null, edits: Edit[], id: string): Edit[] {
  if (!edits.some((e) => e.id === id)) return edits;
  // A later edit may name a table name the removed edit gave (a rename's target): it is re-addressed, not dropped.
  const names = tableNames(base, opsOf(edits));
  const enums = enumAssumptions(opsOf(edits));
  const invalidOf = (es: Edit[]) => {
    const ops = addressOps(base, opsOf(es), names);
    return simulate(base, ops).invalid + enumInvalid(ops, enums);
  };
  // How many of each edit's operations failed to apply in the original sequence.
  const failedBefore = new Map<string, number>();
  let prefix: Edit[] = [];
  let invalid = 0;
  for (const e of edits) {
    const next = invalidOf([...prefix, e]);
    failedBefore.set(e.id, next - invalid);
    prefix = [...prefix, e];
    invalid = next;
  }
  const kept: Edit[] = [];
  invalid = 0;
  for (const e of edits) {
    if (e.id === id) continue;
    const next = invalidOf([...kept, e]);
    if (next - invalid > (failedBefore.get(e.id) ?? 0)) continue;
    kept.push(e);
    invalid = next;
  }
  return addressEdits(base, kept, names);
}

// ---------------------------------------------------------------------------------------------
// Enum operations (the schema's types changeset): which ones still apply
// ---------------------------------------------------------------------------------------------

interface EnumAssumptions {
  /** Enums (`schema.name`) the changeset creates or renames into: they don't exist beforehand. */
  absent: Set<string>;
  /** Per existing enum, the values the changeset adds or renames into: they don't exist beforehand. */
  absentValues: Map<string, Set<string>>;
}

function enumKey(schema: string, name: string): string {
  return `${schema}.${name}`;
}

/** What the database holds before `ops` run, as far as the ops tell: what they create can't exist yet. */
function enumAssumptions(ops: Operation[]): EnumAssumptions {
  const absent = new Set<string>();
  const absentValues = new Map<string, Set<string>>();
  const value = (k: string, v: string) => {
    if (!absentValues.has(k)) absentValues.set(k, new Set());
    absentValues.get(k)!.add(v);
  };
  for (const op of ops) {
    if ("create_enum" in op) absent.add(enumKey(op.create_enum.schema, op.create_enum.name));
    else if ("rename_enum" in op) absent.add(enumKey(op.rename_enum.schema, op.rename_enum.to));
    else if ("add_enum_value" in op) value(enumKey(op.add_enum_value.schema, op.add_enum_value.name), op.add_enum_value.value);
    else if ("rename_enum_value" in op) value(enumKey(op.rename_enum_value.schema, op.rename_enum_value.name), op.rename_enum_value.to);
  }
  return { absent, absentValues };
}

/** One enum during the walk: `values` holds what is known (true: present); `exact` when every value is known. */
interface EnumState {
  values: Map<string, boolean>;
  exact: boolean;
}

/** How many enum operations of `ops` name an enum or value that doesn't exist (or create one that does). */
function enumInvalid(ops: Operation[], assume: EnumAssumptions): number {
  const state = new Map<string, EnumState | null>(); // null: known not to exist
  const get = (k: string): EnumState | null => {
    if (state.has(k)) return state.get(k)!;
    if (assume.absent.has(k)) return null;
    const s: EnumState = { values: new Map([...(assume.absentValues.get(k) ?? [])].map((v) => [v, false])), exact: false };
    state.set(k, s);
    return s;
  };
  const has = (s: EnumState, v: string) => s.values.get(v) ?? !s.exact;
  let invalid = 0;
  for (const op of ops) {
    let ok = true;
    if ("create_enum" in op) {
      const k = enumKey(op.create_enum.schema, op.create_enum.name);
      if (get(k) !== null) ok = false;
      else state.set(k, { values: new Map(op.create_enum.values.map((v) => [v, true])), exact: true });
    } else if ("add_enum_value" in op) {
      const s = get(enumKey(op.add_enum_value.schema, op.add_enum_value.name));
      if (s === null || has(s, op.add_enum_value.value)) ok = false;
      else s.values.set(op.add_enum_value.value, true);
    } else if ("rename_enum_value" in op) {
      const r = op.rename_enum_value;
      const s = get(enumKey(r.schema, r.name));
      if (s === null || !has(s, r.from) || has(s, r.to)) ok = false;
      else {
        s.values.set(r.from, false);
        s.values.set(r.to, true);
      }
    } else if ("rename_enum" in op) {
      const r = op.rename_enum;
      const from = enumKey(r.schema, r.name);
      const to = enumKey(r.schema, r.to);
      const s = get(from);
      if (s === null || get(to) !== null) ok = false;
      else {
        // Values the changeset adds under the new name don't exist yet either.
        for (const v of assume.absentValues.get(to) ?? []) if (!s.values.has(v)) s.values.set(v, false);
        state.set(to, s);
        state.set(from, null);
      }
    } else if ("drop_enum" in op) {
      const k = enumKey(op.drop_enum.schema, op.drop_enum.name);
      if (get(k) === null) ok = false;
      else state.set(k, null);
    } else continue;
    if (!ok) invalid++;
  }
  return invalid;
}
