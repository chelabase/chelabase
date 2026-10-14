import type { ColumnChange, FkAction, Operation, TableRef, TypeEntry, TypeSpec } from "@chelabase/management-client";
import type { ProjectedColumn, ProjectedTable } from "./changeset";

/** The column panel's form, for a new column or an existing one. */
export interface ColumnForm {
  name: string;
  type: { schema: string; name: string } | null;
  array: boolean;
  defaultKind: "none" | "value" | "expression";
  defaultText: string;
  notNull: boolean;
  unique: boolean;
  primaryKey: boolean;
  references: { schema: string; table: string; column: string; onDelete: FkAction | null } | null;
  check: string;
  comment: string;
}

export interface FieldError {
  field: "name" | "type" | "defaultText" | "check" | "references";
  message: string;
}

export function emptyColumnForm(): ColumnForm {
  return {
    name: "",
    type: null,
    array: false,
    defaultKind: "none",
    defaultText: "",
    notNull: false,
    unique: false,
    primaryKey: false,
    references: null,
    check: "",
    comment: "",
  };
}

/** A SQL string literal: `it's` → `'it''s'` (Postgres casts it to the column's type). */
export function sqlLiteral(text: string): string {
  return `'${text.replace(/'/g, "''")}'`;
}

function singleColumn(table: ProjectedTable, kind: "unique" | "primary_key", column: string) {
  return table.constraints.find((c) => c.kind === kind && c.mark !== "dropped" && c.columns.length === 1 && c.columns[0] === column);
}

/** The form for an existing (or pending) column; its default shows as an expression. */
export function formFromColumn(col: ProjectedColumn, table: ProjectedTable, catalog: TypeEntry[]): ColumnForm {
  let type: ColumnForm["type"] = null;
  let array = false;
  if (col.type_spec) {
    type = { schema: col.type_spec.schema, name: col.type_spec.name };
    array = col.type_spec.array ?? false;
  } else if (col.type_oid !== null) {
    const entry = catalog.find((t) => t.oid === col.type_oid);
    const element = entry ? undefined : catalog.find((t) => t.array_oid === col.type_oid);
    const found = entry ?? element;
    if (found) {
      type = { schema: found.schema, name: found.name };
      array = element !== undefined;
    }
  }
  return {
    name: col.name,
    type,
    array,
    defaultKind: col.default === null ? "none" : "expression",
    defaultText: col.default ?? "",
    notNull: col.not_null,
    unique: singleColumn(table, "unique", col.name) !== undefined,
    primaryKey: singleColumn(table, "primary_key", col.name) !== undefined,
    references: null,
    check: "",
    comment: col.comment ?? "",
  };
}

const NAME_BYTES = 63;

export function validateColumnForm(form: ColumnForm, ctx: { table: ProjectedTable; editing: string | null; catalog: TypeEntry[] }): FieldError[] {
  const errors: FieldError[] = [];
  const name = form.name;
  if (name.trim() === "") errors.push({ field: "name", message: "Name the column" });
  else if (new TextEncoder().encode(name).length > NAME_BYTES) errors.push({ field: "name", message: "Postgres names are at most 63 bytes" });
  else if (name !== ctx.editing && ctx.table.columns.some((c) => c.mark !== "dropped" && c.name === name)) {
    errors.push({ field: "name", message: `${name} is already a column` });
  }
  const t = form.type;
  if (t === null || (ctx.catalog.length > 0 && !ctx.catalog.some((e) => e.schema === t.schema && e.name === t.name))) {
    errors.push({ field: "type", message: "Pick a type" });
  }
  if (form.defaultKind === "expression" && form.defaultText.trim() === "") {
    errors.push({ field: "defaultText", message: "Write the default expression" });
  }
  if (form.references !== null && (form.references.table === "" || form.references.column === "")) {
    errors.push({ field: "references", message: "Pick the referenced table and column" });
  }
  return errors;
}

/** `format_type` spellings of pg_catalog types, by catalog name. */
const SQL_NAMES: Record<string, string> = {
  bigint: "int8",
  integer: "int4",
  smallint: "int2",
  boolean: "bool",
  real: "float4",
  "double precision": "float8",
  "character varying": "varchar",
  character: "bpchar",
  "bit varying": "varbit",
  "timestamp without time zone": "timestamp",
  "timestamp with time zone": "timestamptz",
  "time without time zone": "time",
  "time with time zone": "timetz",
};

function unquote(s: string): string {
  return s.startsWith('"') && s.endsWith('"') ? s.slice(1, -1).replace(/""/g, '"') : s;
}

/** Whether `format_type` text names the same type (ignoring modifiers) as the form's choice. */
function sameType(dataType: string, t: { schema: string; name: string }, array: boolean): boolean {
  let text = dataType.trim();
  const isArray = text.endsWith("[]");
  if (isArray) text = text.slice(0, -2);
  if (isArray !== array) return false;
  text = text.replace(/\([^)]*\)/g, " ").replace(/\s+/g, " ").trim();
  const dot = text.match(/^("(?:[^"]|"")+"|[^".]+)\.(.+)$/);
  if (dot) return unquote(dot[1]) === t.schema && unquote(dot[2]) === t.name;
  const name = unquote(text);
  return (SQL_NAMES[name] ?? name) === t.name;
}

function typeSpec(form: ColumnForm): TypeSpec {
  return { schema: form.type!.schema, name: form.type!.name, array: form.array };
}

function desiredDefault(form: ColumnForm): string | null {
  if (form.defaultKind === "value") return sqlLiteral(form.defaultText);
  if (form.defaultKind === "expression") return form.defaultText;
  return null;
}

function constraintOps(form: ColumnForm, table: TableRef, name: string, opts: { pk: boolean; unique: boolean }): Operation[] {
  const ops: Operation[] = [];
  if (opts.pk) ops.push({ add_constraint: { table, constraint: { kind: { primary_key: { columns: [name] } } } } });
  if (opts.unique) ops.push({ add_constraint: { table, constraint: { kind: { unique: { columns: [name] } } } } });
  const r = form.references;
  if (r) {
    ops.push({
      add_constraint: {
        table,
        constraint: {
          kind: {
            foreign_key: {
              columns: [name],
              references: { schema: r.schema, name: r.table },
              referenced_columns: [r.column],
              ...(r.onDelete ? { on_delete: r.onDelete } : {}),
            },
          },
        },
      },
    });
  }
  if (form.check.trim() !== "") ops.push({ add_constraint: { table, constraint: { kind: { check: { expression: form.check } } } } });
  return ops;
}

/**
 * The operations a submitted form generates. A new column: `add_column`, then primary key, unique, foreign
 * key and check constraints. An existing one: one `alter_column` with the changed fields, a `rename_column`,
 * then constraint changes.
 */
export function columnFormOps(form: ColumnForm, ctx: { table: TableRef; editing: ProjectedColumn | null; projected: ProjectedTable }): Operation[] {
  const { table, editing, projected } = ctx;
  const comment = form.comment === "" ? null : form.comment;
  const def = desiredDefault(form);
  if (editing === null) {
    return [
      { add_column: { table, column: { name: form.name, data_type: typeSpec(form), not_null: form.notNull, default: def, comment } } },
      ...constraintOps(form, table, form.name, { pk: form.primaryKey, unique: form.unique }),
    ];
  }

  const ops: Operation[] = [];
  const change: ColumnChange = {};
  if (form.type) {
    const t = editing.type_spec;
    const unchanged = t
      ? t.schema === form.type.schema && t.name === form.type.name && (t.array ?? false) === form.array
      : sameType(editing.data_type, form.type, form.array);
    if (!unchanged) change.data_type = typeSpec(form);
  }
  if (form.notNull !== editing.not_null) change.not_null = form.notNull;
  if (def !== editing.default) change.default = def === null ? "drop" : { set: def };
  if (comment !== editing.comment) change.comment = comment === null ? "clear" : { set: comment };
  if (Object.keys(change).length > 0) ops.push({ alter_column: { table, column: editing.name, change } });
  if (form.name !== editing.name) ops.push({ rename_column: { table, column: editing.name, to: form.name } });

  const pk = singleColumn(projected, "primary_key", editing.name);
  const unique = singleColumn(projected, "unique", editing.name);
  const adds = constraintOps(form, table, form.name, { pk: form.primaryKey && !pk, unique: form.unique && !unique });
  if (!form.primaryKey && pk) ops.push({ drop_constraint: { table, name: pk.name } });
  if (!form.unique && unique) ops.push({ drop_constraint: { table, name: unique.name } });
  return [...ops, ...adds];
}
