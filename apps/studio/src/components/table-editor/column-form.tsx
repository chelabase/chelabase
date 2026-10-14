import { useEffect, useId, useMemo, useRef, useState } from "react";
import { useGetTable, useListObjects, type FkAction } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { RadioGroup } from "@/components/ui/radio-group";
import { addressOps, opsOf, typeSpecText, type Edit } from "@/lib/table-editor/changeset";
import { columnFormOps, emptyColumnForm, formFromColumn, validateColumnForm, type ColumnForm, type FieldError } from "@/lib/table-editor/column-form";
import { useTableEditor, type FormField } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { CheckField, DryRunSummary, inputClass, SqlStatements } from "./panel-bits";
import { Picker } from "./picker";
import type { TableTarget } from "./target";
import { useDryRun } from "./use-dry-run";
import { useTableStructure } from "./use-structure";

export type { FormField } from "@/lib/table-editor/store";

function ErrorLine({ id, error }: { id: string; error: FieldError | undefined }) {
  if (!error) return null;
  return (
    <p id={id} className="text-xs text-coral">
      {error.message}
    </p>
  );
}

/** Suggested default expressions per type name. */
const SUGGESTIONS: Record<string, string[]> = {
  timestamptz: ["now()"],
  timestamp: ["now()"],
  date: ["current_date"],
  time: ["current_time"],
  timetz: ["current_time"],
  uuid: ["gen_random_uuid()"],
  bool: ["true", "false"],
  jsonb: ["'{}'::jsonb"],
  json: ["'{}'::json"],
};

export const ON_DELETE: { value: string; label: string }[] = [
  { value: "", label: "No action (default)" },
  { value: "restrict", label: "Restrict" },
  { value: "cascade", label: "Cascade" },
  { value: "set_null", label: "Set null" },
  { value: "set_default", label: "Set default" },
];

export interface ColumnFormPanelProps {
  target: TableTarget;
  /** The column being edited (its current name); null for a new column. */
  column: string | null;
  /** Why structure can't be changed here, or null. */
  reason: string | null;
  totalBytes: number | null;
}

/**
 * The column form: a new column, or an existing (or pending) one. Its operations are previewed with
 * a dry run after the pending edits, and "Add to changes" stages them as one edit.
 */
export function ColumnFormPanel(props: ColumnFormPanelProps) {
  const { target, column } = props;
  const { base, catalog, projected } = useTableStructure(target);
  const editing = column === null ? null : (projected?.columns.find((c) => c.name === column && c.mark !== "dropped") ?? null);
  // The form starts once the table and the catalog are read (an existing column's type comes from the catalog).
  if (!base || !projected || (column !== null && !editing) || (column !== null && catalog.length === 0)) {
    return (
      <p role="status" className="text-sm text-chalk-dim">
        {base && projected && column !== null && !editing ? `${column} is no longer a column of ${target.schema}.${target.table}.` : "Loading…"}
      </p>
    );
  }
  return <FormBody {...props} initial={editing ? formFromColumn(editing, projected, catalog) : emptyColumnForm()} />;
}

function FormBody({ target, column, reason: structureReason, totalBytes, initial }: ColumnFormPanelProps & { initial: ColumnForm }) {
  const { catalog, projected, edits, ref, stage, base, applying, reason: dropped } = useTableStructure(target);
  const reason = structureReason ?? dropped;
  const openPanel = useTableEditor((s) => s.openPanel);
  const focus = useTableEditor((s) => s.panel.focus);
  const consumeFocus = useTableEditor((s) => s.consumeFocus);
  const [form, setForm] = useState<ColumnForm>(initial);
  const [showErrors, setShowErrors] = useState(false);
  const [refsOpen, setRefsOpen] = useState(false);
  const [checkOpen, setCheckOpen] = useState(form.check !== "");
  const ids = { name: useId(), type: useId(), def: useId(), comment: useId(), check: useId(), err: useId() };
  const fields = useRef<Partial<Record<FormField, HTMLElement | null>>>({});
  const set = (patch: Partial<ColumnForm>) => setForm((f) => ({ ...f, ...patch }));

  const editing = column === null ? null : (projected!.columns.find((c) => c.name === column && c.mark !== "dropped") ?? null);
  const disabled = reason !== null;

  // Focus: Name when the form opens, or the field the panel's focus request names (also on an open form).
  const focused = useRef(false);
  useEffect(() => {
    if (focus === undefined && focused.current) return;
    focused.current = true;
    const el = fields.current[focus?.field ?? "name"];
    if (el) el.focus();
    else fields.current.name?.focus();
    if (focus !== undefined) consumeFocus();
  }, [focus, consumeFocus]);

  const errors = validateColumnForm(form, { table: projected!, editing: editing?.name ?? null, catalog });
  const errorOf = (f: FieldError["field"]) => (showErrors ? errors.find((e) => e.field === f) : undefined);
  const ops = useMemo(
    () => (errors.length === 0 ? columnFormOps(form, { table: ref, editing, projected: projected! }) : []),
    // `errors` follows from `form`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [form, ref, editing, projected],
  );
  // The preview runs this edit after the pending ones (it may build on them), and shows only its statements.
  const [previewCascade, setPreviewCascade] = useState<string | null>(null);
  // Its operations name the table as it is after the pending renames and moves (I1), as Add to changes would.
  const previewEdits = useMemo<Edit[]>(() => {
    if (ops.length === 0) return [];
    const addressed = addressOps(base ?? null, [...opsOf(edits), ...ops]).slice(-ops.length);
    return [...edits, { id: "preview", ops: addressed, label: "" }];
  }, [base, edits, ops]);
  const preview = useDryRun({
    databaseId: target.databaseId,
    branchId: target.branchId,
    database: target.database,
    role: target.role,
    edits: previewEdits,
    table: `${target.schema}.${target.table}`,
    totalBytes,
    enabled: !disabled && !!base && ops.length > 0,
    cascade: previewCascade,
  });
  const mine = edits.length;
  // A pending drop with dependents refuses the (rolled-back) preview without cascade: preview with its
  // hash, so the form shows its own column's outcome. The real Apply still asks in Pending changes.
  const earlierDeps =
    (preview.failure?.code === "has_dependents" || preview.failure?.code === "dependents_changed") && preview.failure.dependents && preview.failure.edit !== null && preview.failure.edit < mine ? (preview.failure.dependents?.hash ?? null) : null;
  useEffect(() => {
    if (earlierDeps !== null && earlierDeps !== previewCascade) setPreviewCascade(earlierDeps);
  }, [earlierDeps, previewCascade]);
  const previewShown = earlierDeps !== null ? { ...preview, failure: null, view: null, running: true } : preview;

  const typeOptions = useMemo(
    () =>
      [...catalog]
        .map((t) => ({ value: `${t.schema}.${t.name}`, label: typeSpecText({ schema: t.schema, name: t.name }) }))
        .sort((a, b) => Number(a.value.startsWith("pg_catalog.") ? 0 : 1) - Number(b.value.startsWith("pg_catalog.") ? 0 : 1) || a.label.localeCompare(b.label)),
    [catalog],
  );
  const pkElsewhere = projected!.constraints.some(
    (k) => k.kind === "primary_key" && k.mark !== "dropped" && !(k.columns.length === 1 && k.columns[0] === editing?.name),
  );
  const suggestions = form.type ? (SUGGESTIONS[form.type.name] ?? []) : [];

  function submit() {
    setShowErrors(true);
    if (errors.length > 0) {
      const first = errors[0].field;
      const el = first === "name" ? fields.current.name : first === "type" ? fields.current.type : first === "defaultText" ? fields.current.default : null;
      el?.focus();
      return;
    }
    if (ops.length > 0) stage(ops);
    openPanel("changes");
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3">
      <div className="space-y-1">
        <label htmlFor={ids.name} className="text-xs text-chalk-dim">
          Name
        </label>
        <input
          id={ids.name}
          ref={(el) => {
            fields.current.name = el;
          }}
          value={form.name}
          disabled={disabled}
          autoComplete="off"
          spellCheck={false}
          aria-invalid={errorOf("name") ? true : undefined}
          aria-describedby={errorOf("name") ? `${ids.err}-name` : undefined}
          onChange={(e) => set({ name: e.target.value })}
          className={cn(inputClass, "font-mono text-[13px]")}
        />
        <ErrorLine id={`${ids.err}-name`} error={errorOf("name")} />
      </div>

      <div className="flex items-end gap-3">
        <div className="min-w-0 flex-1 space-y-1">
          <span aria-hidden="true" className="block text-xs text-chalk-dim">
            Type
          </span>
          <div
            ref={(el) => {
              fields.current.type = el?.querySelector("input") ?? null;
            }}
          >
            <Picker
              label="Type"
              look="field"
              mono
              disabled={disabled}
              options={typeOptions}
              value={form.type ? `${form.type.schema}.${form.type.name}` : null}
              onChange={(v) => {
                const t = catalog.find((e) => `${e.schema}.${e.name}` === v);
                if (t) set({ type: { schema: t.schema, name: t.name } });
              }}
              placeholder="Pick a type"
              describedBy={errorOf("type") ? `${ids.err}-type` : undefined}
            />
          </div>
          <ErrorLine id={`${ids.err}-type`} error={errorOf("type")} />
        </div>
        <CheckField label="Array" checked={form.array} disabled={disabled} onChange={(array) => set({ array })} />
      </div>

      <div
        className="space-y-1.5"
        ref={(el) => {
          fields.current.default = el?.querySelector<HTMLElement>("[role=radio][data-checked], [role=radio]") ?? null;
        }}
      >
        <RadioGroup
          label="Default"
          layout="segmented"
          tone="tab"
          disabled={disabled}
          value={form.defaultKind}
          onValueChange={(defaultKind) => set({ defaultKind })}
          options={[
            { value: "none", label: "None" },
            { value: "value", label: "Value" },
            { value: "expression", label: "Expression" },
          ]}
        />
        {form.defaultKind !== "none" && (
          <input
            id={ids.def}
            aria-label={form.defaultKind === "value" ? "Default value" : "Default expression"}
            value={form.defaultText}
            disabled={disabled}
            autoComplete="off"
            spellCheck={false}
            aria-invalid={errorOf("defaultText") ? true : undefined}
            aria-describedby={errorOf("defaultText") ? `${ids.err}-def` : undefined}
            onChange={(e) => set({ defaultText: e.target.value })}
            placeholder={form.defaultKind === "value" ? "A value, stored as typed" : "SQL, such as now()"}
            className={cn(inputClass, "font-mono text-[13px]")}
          />
        )}
        {suggestions.length > 0 && !disabled && (
          <div className="flex flex-wrap gap-3 font-mono text-xs">
            {suggestions.map((s) => (
              <button
                key={s}
                type="button"
                onClick={() => set({ defaultKind: "expression", defaultText: s })}
                className="text-chalk-dim underline decoration-line-strong underline-offset-[3px] outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand"
              >
                {s}
              </button>
            ))}
          </div>
        )}
        <ErrorLine id={`${ids.err}-def`} error={errorOf("defaultText")} />
      </div>

      <fieldset className="space-y-0">
        <legend className="mb-1 text-xs text-chalk-dim">Constraints</legend>
        <CheckField label="Not null" checked={form.notNull} disabled={disabled} onChange={(notNull) => set({ notNull })} />
        <CheckField label="Unique" checked={form.unique} disabled={disabled} onChange={(unique) => set({ unique })} />
        <CheckField
          label="Primary key"
          checked={form.primaryKey}
          disabled={disabled || (pkElsewhere && !form.primaryKey)}
          note={pkElsewhere && !form.primaryKey ? "the table already has one" : undefined}
          onChange={(primaryKey) => set({ primaryKey })}
        />
      </fieldset>

      <div className="flex flex-wrap gap-2">
        <Button
          size="sm"
          variant="outline"
          disabled={disabled}
          aria-expanded={refsOpen || form.references !== null}
          onClick={() => {
            if (refsOpen || form.references) {
              setRefsOpen(false);
              set({ references: null });
            } else {
              setRefsOpen(true);
              set({ references: { schema: target.schema, table: "", column: "", onDelete: null } });
            }
          }}
        >
          References…
        </Button>
        <Button size="sm" variant="outline" disabled={disabled} aria-expanded={checkOpen}
          onClick={() => {
            // Closing drops the expression: a hidden check is never staged.
            if (checkOpen) set({ check: "" });
            setCheckOpen(!checkOpen);
          }}
        >
          Check…
        </Button>
      </div>
      {form.references && (
        <ReferencesFields target={target} value={form.references} disabled={disabled} onChange={(references) => set({ references })} error={errorOf("references")} />
      )}
      {checkOpen && (
        <div className="space-y-1">
          <label htmlFor={ids.check} className="text-xs text-chalk-dim">
            Check expression
          </label>
          <textarea
            id={ids.check}
            value={form.check}
            disabled={disabled}
            spellCheck={false}
            rows={2}
            onChange={(e) => set({ check: e.target.value })}
            placeholder={form.name ? `${form.name} >= 0` : "amount >= 0"}
            className={cn(inputClass, "h-auto py-1.5 font-mono text-[13px]")}
          />
        </div>
      )}

      <div className="space-y-1">
        <label htmlFor={ids.comment} className="text-xs text-chalk-dim">
          Comment
        </label>
        <input
          id={ids.comment}
          ref={(el) => {
            fields.current.comment = el;
          }}
          value={form.comment}
          disabled={disabled}
          onChange={(e) => set({ comment: e.target.value })}
          className={inputClass}
        />
      </div>

      <section aria-label="SQL preview" className="space-y-2">
        {ops.length === 0 ? (
          <p className="text-[13px] text-chalk-dim">
            {errors.length > 0 ? "The SQL shows once the column has a name and a type." : "Nothing changes yet."}
          </p>
        ) : (
          <>
            <SqlStatements statements={(previewShown.view?.statements ?? []).filter((s) => s.edit === mine)} pending={previewShown.running && !previewShown.view} />
            <DryRunSummary state={previewShown} edit={mine} />
          </>
        )}
      </section>

      {reason && <p className="text-[13px] text-chalk-dim">{reason}</p>}
      <div className="mt-auto flex justify-end gap-2 pt-2">
        <Button variant="ghost" onClick={() => openPanel("closed")}>
          Cancel
        </Button>
        <Button disabled={disabled || applying} onClick={submit}>
          Add to changes
        </Button>
      </div>
    </div>
  );
}

function ReferencesFields({
  target,
  value,
  onChange,
  disabled,
  error,
}: {
  target: TableTarget;
  value: NonNullable<ColumnForm["references"]>;
  onChange: (v: ColumnForm["references"]) => void;
  disabled: boolean;
  error: FieldError | undefined;
}) {
  const params = target.database ? { database: target.database } : undefined;
  const objects = useListObjects(target.databaseId, target.branchId, params);
  const tables = (objects.data?.objects ?? []).filter((o) => o.kind === "table" || o.kind === "partitioned_table");
  const referenced = useGetTable(target.databaseId, target.branchId, value.schema, value.table, params, { query: { enabled: value.table !== "" } });
  const columns = [...(referenced.data?.columns ?? [])].sort((a, b) => a.attnum - b.attnum);
  return (
    <div className="space-y-2 border-l-2 border-line-strong pl-3">
      <div className="space-y-1">
        <span aria-hidden="true" className="block text-xs text-chalk-dim">
          Referenced table
        </span>
        <Picker
          label="Referenced table"
          look="field"
          mono
          disabled={disabled}
          options={tables.map((o) => ({ value: `${o.schema}.${o.name}`, label: o.schema === target.schema ? o.name : `${o.schema}.${o.name}` }))}
          value={value.table ? `${value.schema}.${value.table}` : null}
          onChange={(v) => {
            const o = tables.find((t) => `${t.schema}.${t.name}` === v);
            if (o) onChange({ ...value, schema: o.schema, table: o.name, column: "" });
          }}
          placeholder="Pick a table"
        />
      </div>
      <div className="space-y-1">
        <span aria-hidden="true" className="block text-xs text-chalk-dim">
          Referenced column
        </span>
        <Picker
          label="Referenced column"
          look="field"
          mono
          disabled={disabled || value.table === ""}
          options={columns.map((c) => ({ value: c.name, label: c.name, note: c.data_type }))}
          value={value.column || null}
          onChange={(column) => onChange({ ...value, column })}
          placeholder="Pick a column"
        />
      </div>
      <div className="space-y-1">
        <span aria-hidden="true" className="block text-xs text-chalk-dim">
          On delete
        </span>
        <Picker
          label="On delete"
          look="field"
          disabled={disabled}
          options={ON_DELETE}
          value={value.onDelete ?? ""}
          onChange={(v) => onChange({ ...value, onDelete: v === "" ? null : (v as FkAction) })}
        />
      </div>
      {error && <p className="text-xs text-coral">{error.message}</p>}
    </div>
  );
}
