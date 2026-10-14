import type { ConstraintKindSpec, FkAction, Operation, TableRef } from "@chelabase/management-client";
import type { ProjectedTable } from "./changeset";

export type ConstraintFormKind = "primary_key" | "unique" | "foreign_key" | "check";

export interface ConstraintForm {
  kind: ConstraintFormKind;
  columns: string[];
  /** The referenced table, `schema.name`; null until picked. */
  refTable: { schema: string; name: string } | null;
  refColumns: string[];
  onDelete: FkAction | null;
  expression: string;
  notValid: boolean;
  /** Empty: Postgres names it. */
  name: string;
}

export interface ConstraintFieldError {
  field: "columns" | "references" | "expression" | "name";
  message: string;
}

export const emptyConstraintForm = (): ConstraintForm => ({
  kind: "unique",
  columns: [],
  refTable: null,
  refColumns: [],
  onDelete: null,
  expression: "",
  notValid: false,
  name: "",
});

/** Postgres accepts NOT VALID on foreign keys and checks only. */
export const canBeNotValid = (kind: ConstraintFormKind) => kind === "foreign_key" || kind === "check";

/**
 * `relationNames`: the names of the tables, views and indexes in the table's schema. A primary key or
 * unique constraint creates an index under its name, which must not be taken by another relation there.
 */
export function validateConstraintForm(form: ConstraintForm, table: ProjectedTable, relationNames: ReadonlySet<string> = new Set()): ConstraintFieldError[] {
  const errors: ConstraintFieldError[] = [];
  if (form.kind !== "check" && form.columns.length === 0) errors.push({ field: "columns", message: "Pick at least one column" });
  if (form.kind === "primary_key" && table.constraints.some((c) => c.kind === "primary_key" && c.mark !== "dropped")) {
    errors.push({ field: "columns", message: `${table.name} already has a primary key` });
  }
  if (form.kind === "foreign_key" && (!form.refTable || form.refColumns.length === 0 || form.refColumns.length !== form.columns.length)) {
    errors.push({ field: "references", message: form.columns.length > 1 ? `Pick ${form.columns.length} referenced columns` : "Pick the referenced table and column" });
  }
  if (form.kind === "check" && form.expression.trim() === "") errors.push({ field: "expression", message: "Write the check expression" });
  const name = form.name.trim();
  if (name !== "") {
    if (new TextEncoder().encode(name).length > 63) errors.push({ field: "name", message: "Postgres names are at most 63 bytes" });
    else if (table.constraints.some((c) => c.name === name && c.mark !== "dropped")) errors.push({ field: "name", message: `${name} is already a constraint` });
    else if ((form.kind === "primary_key" || form.kind === "unique") && relationNames.has(name)) {
      errors.push({ field: "name", message: `${name} is already the name of a table, view or index in ${table.schema}` });
    }
  }
  return errors;
}

/** The `add_constraint` a valid form stands for. */
export function constraintFormOps(form: ConstraintForm, table: TableRef): Operation[] {
  let kind: ConstraintKindSpec;
  if (form.kind === "primary_key") kind = { primary_key: { columns: form.columns } };
  else if (form.kind === "unique") kind = { unique: { columns: form.columns } };
  else if (form.kind === "check") kind = { check: { expression: form.expression.trim() } };
  else {
    kind = {
      foreign_key: {
        columns: form.columns,
        references: { schema: form.refTable!.schema, name: form.refTable!.name },
        referenced_columns: form.refColumns,
        ...(form.onDelete ? { on_delete: form.onDelete } : {}),
      },
    };
  }
  const name = form.name.trim();
  return [{ add_constraint: { table, constraint: { kind, ...(name ? { name } : {}), ...(form.notValid && canBeNotValid(form.kind) ? { not_valid: true } : {}) } } }];
}
