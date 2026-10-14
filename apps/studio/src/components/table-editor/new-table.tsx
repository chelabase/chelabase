import { useId, useMemo, useState } from "react";
import { Plus, X } from "lucide-react";
import { useListObjects, type ColumnSpec, type Operation } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { typeSpecText } from "@/lib/table-editor/changeset";
import { schemasOf } from "@/lib/table-editor/context";
import { useTableEditor } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { isIdentifier, utf8Length } from "./edit-dialog";
import { CheckField, inputClass } from "./panel-bits";
import { Picker } from "./picker";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

interface DraftColumn {
  key: number;
  name: string;
  type: { schema: string; name: string } | null;
  array: boolean;
  notNull: boolean;
  /** SQL expression, or empty. */
  default: string;
  identity: boolean;
  primaryKey: boolean;
}

const starterColumns = (): DraftColumn[] => [
  { key: 1, name: "id", type: { schema: "pg_catalog", name: "int8" }, array: false, notNull: true, default: "", identity: true, primaryKey: true },
  { key: 2, name: "created_at", type: { schema: "pg_catalog", name: "timestamptz" }, array: false, notNull: true, default: "now()", identity: false, primaryKey: false },
];

function columnSpec(c: DraftColumn): ColumnSpec {
  return {
    name: c.name.trim(),
    data_type: { schema: c.type!.schema, name: c.type!.name, array: c.array },
    ...(c.identity ? { identity: "always" as const } : {}),
    ...(c.notNull ? { not_null: true } : {}),
    ...(c.default.trim() !== "" && !c.identity ? { default: c.default.trim() } : {}),
  };
}

const flags = (c: DraftColumn) =>
  [c.primaryKey && "primary key", c.identity && "identity", c.notNull && "not null", c.default.trim() !== "" && `default ${c.default.trim()}`].filter((f): f is string => !!f);

/**
 * The New table view: a name, a schema, starter columns (`id`, `created_at`), a comment and "Turn RLS on"
 * (unticked by default, R8). Create table stages one `create_table` in the schema's `#new` changeset. A table
 * in a schema the data API serves gets its default grants (anon reads, authenticated reads and writes), so
 * with RLS off those roles see every row: the form says so.
 */
export function NewTableView({ target, reason }: { target: TableTarget; reason: string | null }) {
  const { catalog, changeset, stage, applying } = useTableStructure(target);
  const openPanel = useTableEditor((s) => s.openPanel);
  const params = target.database ? { database: target.database } : undefined;
  const objects = useListObjects(target.databaseId, target.branchId, params);
  const [name, setName] = useState("");
  const [schema, setSchema] = useState(target.schema);
  const [comment, setComment] = useState("");
  const [rls, setRls] = useState(false);
  const [columns, setColumns] = useState<DraftColumn[]>(starterColumns);
  const [editing, setEditing] = useState<number | null>(null);
  const [next, setNext] = useState(3);
  const [tried, setTried] = useState(false);
  const ids = { name: useId(), comment: useId() };

  const staged = (changeset?.edits.length ?? 0) > 0;
  const disabled = reason !== null || applying || staged;
  const all = objects.data?.objects ?? [];
  const schemas = schemasOf(all);
  const typeOptions = useMemo(
    () =>
      [...catalog]
        .map((t) => ({ value: `${t.schema}.${t.name}`, label: typeSpecText({ schema: t.schema, name: t.name }) }))
        .sort((a, b) => Number(a.value.startsWith("pg_catalog.") ? 0 : 1) - Number(b.value.startsWith("pg_catalog.") ? 0 : 1) || a.label.localeCompare(b.label)),
    [catalog],
  );

  const tableName = name.trim();
  const nameError =
    tableName === ""
      ? "Name the table"
      : utf8Length(tableName) > 63
        ? "Postgres names are at most 63 bytes"
        : !isIdentifier(schema)
          ? "Postgres names are at most 63 bytes"
          : all.some((o) => o.schema === schema && o.name === tableName)
          ? `${schema}.${tableName} already exists`
          : null;
  const columnError = (c: DraftColumn): string | null => {
    const n = c.name.trim();
    if (n === "") return "Name the column";
    if (utf8Length(n) > 63) return "Postgres names are at most 63 bytes";
    if (columns.some((o) => o !== c && o.name.trim() === n)) return `${n} is already a column`;
    if (!c.type) return "Pick a type";
    return null;
  };
  const firstBad = columns.find((c) => columnError(c) !== null);
  const listError = columns.length === 0 ? "Add at least one column" : null;
  const valid = nameError === null && listError === null && !firstBad;

  const patch = (key: number, p: Partial<DraftColumn>) => setColumns((cs) => cs.map((c) => (c.key === key ? { ...c, ...p } : c)));

  function create() {
    setTried(true);
    if (!valid) {
      if (firstBad) setEditing(firstBad.key);
      return;
    }
    const pk = columns.filter((c) => c.primaryKey).map((c) => c.name.trim());
    const op: Operation = {
      create_table: {
        table: { schema, name: tableName },
        columns: columns.map(columnSpec),
        ...(pk.length > 0 ? { primary_key: pk } : {}),
        comment: comment.trim() === "" ? null : comment.trim(),
        ...(rls ? { rls: { enabled: true, forced: false } } : {}),
      },
    };
    stage([op]);
    openPanel("changes");
  }

  return (
    <section aria-label="New table" className="min-h-0 flex-1 overflow-auto p-4">
      <div className="max-w-2xl space-y-5">
        <h2 className="text-lg leading-6 font-medium">New table</h2>
        {reason && <p className="text-[13px] text-chalk-dim">{reason}</p>}
        {staged && (
          <p className="text-[13px] text-chalk-dim">
            A new table is already staged for this schema. Apply or discard it in the pending changes to start another.
          </p>
        )}

        <div className="flex gap-3">
          <div className="min-w-0 flex-1 space-y-1">
            <label htmlFor={ids.name} className="block text-xs text-chalk-dim">
              Table name
            </label>
            <input
              id={ids.name}
              value={name}
              disabled={disabled}
              autoComplete="off"
              spellCheck={false}
              aria-invalid={tried && nameError ? true : undefined}
              onChange={(e) => setName(e.target.value)}
              className={cn(inputClass, "font-mono text-[13px]")}
            />
            {tried && nameError && <p className="text-xs text-coral">{nameError}</p>}
          </div>
          <div className="w-48 space-y-1">
            <span aria-hidden="true" className="block text-xs text-chalk-dim">
              Schema
            </span>
            <Picker label="Schema" look="field" mono disabled={disabled} options={(schemas.includes(schema) ? schemas : [schema, ...schemas]).map((s) => ({ value: s, label: s }))} value={schema} onChange={setSchema} acceptTyped={isIdentifier} />
          </div>
        </div>

        <div className="space-y-2">
          <h3 className="text-sm font-semibold text-chalk">Columns</h3>
          <ul className="border-t border-line">
            {columns.map((c) => (
              <li key={c.key} className="border-b border-line py-2">
                {editing === c.key ? (
                  <ColumnEditor
                    column={c}
                    typeOptions={typeOptions}
                    error={tried ? columnError(c) : null}
                    disabled={disabled}
                    onChange={(p) => patch(c.key, p)}
                    onDone={() => setEditing(null)}
                  />
                ) : (
                  <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
                    <span className="font-mono text-[13px] text-chalk">{c.name}</span>
                    <span className="font-mono text-xs text-chalk-dim">{c.type ? `${typeSpecText({ schema: c.type.schema, name: c.type.name })}${c.array ? "[]" : ""}` : "no type"}</span>
                    <span className="text-xs text-chalk-dim">{flags(c).join(" · ")}</span>
                    {tried && columnError(c) && <span className="text-xs text-coral">{columnError(c)}</span>}
                    <span className="ml-auto flex items-baseline gap-2">
                      <button type="button" aria-label={`Edit column ${c.name}`} disabled={disabled} onClick={() => setEditing(c.key)} className="px-1 text-xs text-chalk-dim underline decoration-line-strong underline-offset-[3px] outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand disabled:no-underline disabled:opacity-50">
                        Edit
                      </button>
                      <button
                        type="button"
                        aria-label={`Remove column ${c.name}`}
                        disabled={disabled}
                        onClick={() => {
                          setColumns((cs) => cs.filter((o) => o.key !== c.key));
                          if (editing === c.key) setEditing(null);
                        }}
                        className="flex size-6 items-center justify-center text-chalk-dim outline-none hover:text-chalk focus-visible:outline-2 focus-visible:outline-sand disabled:opacity-50"
                      >
                        <X aria-hidden="true" size={14} strokeWidth={1.5} />
                      </button>
                    </span>
                  </div>
                )}
              </li>
            ))}
          </ul>
          {tried && listError && <p className="text-xs text-coral">{listError}</p>}
          <Button
            size="sm"
            variant="outline"
            disabled={disabled}
            onClick={() => {
              setColumns((cs) => [...cs, { key: next, name: "", type: null, array: false, notNull: false, default: "", identity: false, primaryKey: false }]);
              setEditing(next);
              setNext(next + 1);
            }}
          >
            <Plus aria-hidden="true" strokeWidth={1.5} />
            Add column
          </Button>
        </div>

        <div className="space-y-1">
          <label htmlFor={ids.comment} className="block text-xs text-chalk-dim">
            Table comment
          </label>
          <input id={ids.comment} value={comment} disabled={disabled} onChange={(e) => setComment(e.target.value)} className={inputClass} />
        </div>

        <div className="space-y-1">
          <CheckField label="Turn RLS on" checked={rls} disabled={disabled} onChange={setRls} />
          {rls ? (
            <p className="text-[13px] text-chalk-dim">With RLS on and no policy yet, only the owner and roles that bypass RLS see rows. Add policies once the table exists.</p>
          ) : (
            <p className="text-[13px] text-chalk-dim">
              RLS stays off. If the data API serves {schema}, its default grants let anon read every row and authenticated read and write every row of this table.
            </p>
          )}
        </div>

        <Button disabled={disabled} title={reason ?? undefined} onClick={create}>
          Create table
        </Button>
      </div>
    </section>
  );
}

function ColumnEditor({
  column: c,
  typeOptions,
  error,
  disabled,
  onChange,
  onDone,
}: {
  column: DraftColumn;
  typeOptions: { value: string; label: string }[];
  error: string | null;
  disabled: boolean;
  onChange: (p: Partial<DraftColumn>) => void;
  onDone: () => void;
}) {
  const id = useId();
  return (
    <div className="space-y-2" role="group" aria-label={`Column ${c.name || "(new)"}`}>
      <div className="flex items-end gap-3">
        <div className="min-w-0 flex-1 space-y-1">
          <label htmlFor={`${id}-name`} className="block text-xs text-chalk-dim">
            Column name
          </label>
          <input id={`${id}-name`} autoFocus value={c.name} disabled={disabled} autoComplete="off" spellCheck={false} onChange={(e) => onChange({ name: e.target.value })} className={cn(inputClass, "font-mono text-[13px]")} />
        </div>
        <div className="min-w-0 flex-1">
          <Picker
            label="Column type"
            look="field"
            mono
            disabled={disabled}
            options={typeOptions}
            value={c.type ? `${c.type.schema}.${c.type.name}` : null}
            onChange={(v) => {
              const [schema, ...rest] = v.split(".");
              onChange({ type: { schema, name: rest.join(".") } });
            }}
            placeholder="Pick a type"
          />
        </div>
      </div>
      <div className="flex flex-wrap items-center gap-x-4">
        <CheckField label="Array" checked={c.array} disabled={disabled} onChange={(array) => onChange({ array })} />
        <CheckField label="Not null" checked={c.notNull} disabled={disabled} onChange={(notNull) => onChange({ notNull })} />
        <CheckField label="Primary key" checked={c.primaryKey} disabled={disabled} onChange={(primaryKey) => onChange({ primaryKey, ...(primaryKey ? { notNull: true } : {}) })} />
        <CheckField label="Identity" checked={c.identity} disabled={disabled} onChange={(identity) => onChange({ identity, ...(identity ? { notNull: true } : {}) })} />
      </div>
      <div className="space-y-1">
        <label htmlFor={`${id}-def`} className="block text-xs text-chalk-dim">
          Default expression
        </label>
        <input id={`${id}-def`} value={c.default} disabled={disabled || c.identity} autoComplete="off" spellCheck={false} placeholder="SQL, such as now()" onChange={(e) => onChange({ default: e.target.value })} className={cn(inputClass, "font-mono text-[13px]")} />
      </div>
      {error && <p className="text-xs text-coral">{error}</p>}
      <Button size="sm" variant="outline" onClick={onDone}>
        Done
      </Button>
    </div>
  );
}
