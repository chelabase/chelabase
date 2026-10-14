import { useState } from "react";
import { Plus } from "lucide-react";
import { useGetTable, useListObjects, type FkAction } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { MultiCombobox } from "@/components/ui/combobox";
import type { ProjectedConstraint } from "@/lib/table-editor/changeset";
import {
  canBeNotValid,
  constraintFormOps,
  emptyConstraintForm,
  validateConstraintForm,
  type ConstraintForm,
  type ConstraintFormKind,
} from "@/lib/table-editor/constraint-form";
import { useTableEditor } from "@/lib/table-editor/store";
import { ON_DELETE } from "./column-form";
import { EditDialog, Field, TextField } from "./edit-dialog";
import { CheckField } from "./panel-bits";
import { Picker } from "./picker";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

const KINDS: { value: ConstraintFormKind; label: string }[] = [
  { value: "primary_key", label: "Primary key" },
  { value: "unique", label: "Unique" },
  { value: "foreign_key", label: "Foreign key" },
  { value: "check", label: "Check" },
];

export interface ConstraintsProps {
  target: TableTarget;
  /** Why structure can't be changed here, or null. */
  reason: string | null;
}

/** The Constraints section's `Add constraint` button and its form dialog. */
export function AddConstraintButton({ target, reason }: ConstraintsProps) {
  const [open, setOpen] = useState(false);
  const { applying } = useTableStructure(target);
  return (
    <>
      <Button size="sm" variant="outline" disabled={reason !== null || applying} title={reason ?? undefined} onClick={() => setOpen(true)}>
        <Plus aria-hidden="true" strokeWidth={1.5} />
        Add constraint
      </Button>
      {open && <AddConstraintDialog target={target} onClose={() => setOpen(false)} />}
    </>
  );
}

function AddConstraintDialog({ target, onClose }: { target: TableTarget; onClose: () => void }) {
  const { base, projected, stage } = useTableStructure(target);
  const openPanel = useTableEditor((s) => s.openPanel);
  const [form, setForm] = useState<ConstraintForm>(emptyConstraintForm);
  const set = (patch: Partial<ConstraintForm>) => setForm((f) => ({ ...f, ...patch }));
  const params = target.database ? { database: target.database } : undefined;
  const objects = useListObjects(target.databaseId, target.branchId, params);
  const tables = (objects.data?.objects ?? []).filter((o) => o.kind === "table" || o.kind === "partitioned_table");
  // The table itself, by its current name or (as the object list still names it) its base name.
  const selfRef =
    !!projected &&
    !!form.refTable &&
    ((form.refTable.schema === projected.schema && form.refTable.name === projected.name) || (form.refTable.schema === target.schema && form.refTable.name === target.table));
  const referenced = useGetTable(target.databaseId, target.branchId, form.refTable?.schema ?? "", form.refTable?.name ?? "", params, {
    query: { enabled: !!form.refTable && !selfRef },
  });
  if (!projected) return null;

  const live = projected.columns.filter((c) => c.mark !== "dropped");
  const refColumns = selfRef
    ? live.map((c) => ({ name: c.name, type: c.data_type }))
    : [...(referenced.data?.columns ?? [])].sort((a, b) => a.attnum - b.attnum).map((c) => ({ name: c.name, type: c.data_type }));
  // A primary key or unique constraint creates an index: its name must be free among the schema's relations.
  const relationNames = new Set([
    ...(objects.data?.objects ?? []).filter((o) => o.schema === projected.schema && !(o.schema === target.schema && o.name === target.table && projected.name !== target.table)).map((o) => o.name),
    projected.name,
    // The table's indexes move with it.
    ...(base?.indexes ?? []).map((ix) => ix.name),
  ]);
  const errors = validateConstraintForm(form, projected, relationNames);
  const ref = { schema: projected.schema, name: projected.name };
  const single = form.columns.length <= 1;

  return (
    <EditDialog
      title="Add constraint"
      description={`On ${projected.schema}.${projected.name}`}
      errors={errors}
      onClose={onClose}
      onSubmit={() => {
        stage(constraintFormOps(form, ref));
        openPanel("changes");
        onClose();
      }}
    >
      {(show) => (
        <>
          <Field label="Kind">
            <Picker
              label="Kind"
              look="field"
              options={KINDS}
              value={form.kind}
              onChange={(kind) => set({ kind: kind as ConstraintFormKind, notValid: canBeNotValid(kind as ConstraintFormKind) ? form.notValid : false })}
            />
          </Field>

          {form.kind !== "check" && (
            <div className="space-y-1">
              <MultiCombobox
                label="Columns"
                mono
                // Above the field: the open list never covers the dialog's buttons (M7).
                side="top"
                options={live.map((c) => ({ value: c.name, detail: c.data_type }))}
                value={form.columns}
                onValueChange={(columns) => set({ columns, refColumns: form.refColumns.slice(0, columns.length) })}
                placeholder="Pick columns"
              />
              {show("columns") && <p className="text-xs text-coral">{show("columns")}</p>}
            </div>
          )}

          {form.kind === "foreign_key" && (
            <div className="space-y-2 border-l-2 border-line-strong pl-3">
              <Field label="References table">
                <Picker
                  label="References table"
                  look="field"
                  mono
                  options={tables.map((o) => ({ value: `${o.schema}.${o.name}`, label: o.schema === projected.schema ? o.name : `${o.schema}.${o.name}` }))}
                  value={form.refTable ? `${form.refTable.schema}.${form.refTable.name}` : null}
                  onChange={(v) => {
                    const o = tables.find((t) => `${t.schema}.${t.name}` === v);
                    if (o) set({ refTable: { schema: o.schema, name: o.name }, refColumns: [] });
                  }}
                  placeholder="Pick a table"
                />
              </Field>
              {single ? (
                <Field label="References column">
                  <Picker
                    label="References column"
                    look="field"
                    mono
                    disabled={!form.refTable}
                    options={refColumns.map((c) => ({ value: c.name, label: c.name }))}
                    value={form.refColumns[0] ?? null}
                    onChange={(c) => set({ refColumns: [c] })}
                    placeholder="Pick a column"
                  />
                </Field>
              ) : (
                <MultiCombobox
                  label="References columns"
                  mono
                  side="top"
                  disabled={!form.refTable}
                  options={refColumns.map((c) => ({ value: c.name, detail: c.type }))}
                  value={form.refColumns}
                  onValueChange={(refColumns) => set({ refColumns })}
                  placeholder="In the order of the columns above"
                />
              )}
              <Field label="On delete">
                <Picker
                  label="On delete"
                  look="field"
                  options={ON_DELETE}
                  value={form.onDelete ?? ""}
                  onChange={(v) => set({ onDelete: v === "" ? null : (v as FkAction) })}
                />
              </Field>
              {show("references") && <p className="text-xs text-coral">{show("references")}</p>}
            </div>
          )}

          {form.kind === "check" && (
            <TextField label="Expression" multiline value={form.expression} onChange={(expression) => set({ expression })} error={show("expression")} placeholder="amount_cents > 0" />
          )}

          {canBeNotValid(form.kind) && (
            <div>
              <CheckField label="Add as NOT VALID" checked={form.notValid} onChange={(notValid) => set({ notValid })} />
              <p className="pl-6 text-xs text-chalk-dim">Skips checking the existing rows. Validate it later.</p>
            </div>
          )}

          <TextField label="Name" value={form.name} onChange={(name) => set({ name })} error={show("name")} placeholder="Optional: Postgres picks one" />
        </>
      )}
    </EditDialog>
  );
}

const actionClass =
  "px-1 text-xs text-chalk-dim underline decoration-line-strong underline-offset-[3px] outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand disabled:no-underline disabled:opacity-50";

/** A constraint row's actions: Validate (while not validated), Rename, Drop. */
export function ConstraintActions({ target, constraint, reason }: ConstraintsProps & { constraint: ProjectedConstraint }) {
  const { projected, stage, applying } = useTableStructure(target);
  const openPanel = useTableEditor((s) => s.openPanel);
  const [renaming, setRenaming] = useState(false);
  if (!projected || constraint.mark === "dropped") return null;
  const ref = { schema: projected.schema, name: projected.name };
  const disabled = reason !== null || applying;
  const name = constraint.name;
  const run = (op: "validate" | "drop") => {
    stage([op === "validate" ? { validate_constraint: { table: ref, name } } : { drop_constraint: { table: ref, name } }]);
    openPanel("changes");
  };
  return (
    <span className="ml-auto flex items-baseline gap-2">
      {!constraint.validated && (
        <button type="button" aria-label={`Validate ${name}`} disabled={disabled} onClick={() => run("validate")} className={actionClass}>
          Validate
        </button>
      )}
      <button type="button" aria-label={`Rename ${name}`} disabled={disabled} onClick={() => setRenaming(true)} className={actionClass}>
        Rename
      </button>
      <button type="button" aria-label={`Drop ${name}`} disabled={disabled} onClick={() => run("drop")} className={actionClass}>
        Drop
      </button>
      {renaming && (
        <RenameDialog
          title={`Rename ${name}`}
          taken={(to) => to !== name && projected.constraints.some((c) => c.name === to && c.mark !== "dropped")}
          what="constraint"
          onClose={() => setRenaming(false)}
          onSubmit={(to) => {
            stage([{ rename_constraint: { table: ref, name, to } }]);
            openPanel("changes");
          }}
        />
      )}
    </span>
  );
}

/** The `New name` dialog for renaming a constraint, an enum or the table. */
export function RenameDialog({
  title,
  description,
  what,
  current,
  taken,
  onSubmit,
  onClose,
}: {
  title: string;
  description?: string;
  what: string;
  /** The present name: renaming to it is refused. */
  current?: string;
  taken: (to: string) => boolean;
  onSubmit: (to: string) => void;
  onClose: () => void;
}) {
  const [to, setTo] = useState("");
  const name = to.trim();
  const errors =
    name === ""
      ? [{ field: "name", message: `Name the ${what}` }]
      : new TextEncoder().encode(name).length > 63
        ? [{ field: "name", message: "Postgres names are at most 63 bytes" }]
        : name === current
          ? [{ field: "name", message: `That is already its name` }]
          : taken(name)
            ? [{ field: "name", message: `${name} is already in use` }]
            : [];
  return (
    <EditDialog
      title={title}
      description={description}
      errors={errors}
      onClose={onClose}
      onSubmit={() => {
        onSubmit(name);
        onClose();
      }}
    >
      {(show) => <TextField label="New name" autoFocus value={to} onChange={setTo} error={show("name")} />}
    </EditDialog>
  );
}
