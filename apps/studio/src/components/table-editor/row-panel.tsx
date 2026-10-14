import { useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { consoleQueryExact, useGetTable, useInsertDefaultRow, useListTypes, type ObjectSummary } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { Switch } from "@/components/ui/switch";
import { buildSelect, consoleErrorText, inputKind, parseValue, whereByKey, type Tree } from "@/lib/console";
import type { RowAccess } from "@/lib/table-editor/access";
import type { EditorParams } from "@/lib/table-editor/context";
import { smallColumns } from "@/lib/table-editor/oversized";
import {
  columnDescs,
  editText,
  isTooLarge,
  primaryKeyOf,
  temporalKind,
  validTimestamptz,
  type Row,
  type RowColumn,
  type TemporalKind,
} from "@/lib/table-editor/rows";
import { rowKey } from "@/lib/table-editor/selection";
import { useTableEditor } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { JsonEditor, toInputValue } from "./cell-editor";
import { FkPicker, fkOf } from "./fk-picker";
import { LoadValueButton, cellKey, useLoadValue } from "./load-value";
import { Picker } from "./picker";
import { CollapseButton, CollapsedStrip } from "./panel-bits";
import { PANEL_ROW, ROWS_QUERY, deletedNotice, rowWriteKeys } from "./rows-view";
import type { TableTarget } from "./target";

/** Fields above this many get a "Filter fields" input. */
export const FILTER_FIELDS_ABOVE = 30;

/** What a field holds while it's edited: NULL, or text the column's type parses. */
interface Value {
  null: boolean;
  text: string;
}

interface PanelRow {
  row: Row;
  /** The row was too large: its large columns weren't read ("Load value"). */
  fallback: boolean;
}

const inputClass =
  "h-9 w-full min-w-0 border border-line-strong bg-ink px-2.5 font-mono text-[13px] text-chalk outline-none placeholder:text-chalk-dim read-only:border-transparent read-only:bg-transparent read-only:px-0 read-only:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand disabled:opacity-60 aria-invalid:border-coral";
const linkButton =
  "shrink-0 px-1 text-xs text-chalk underline underline-offset-3 outline-none hover:text-sand focus-visible:outline-2 focus-visible:outline-sand";
const smallButton =
  "h-6 shrink-0 border border-line-strong px-2 text-xs text-chalk outline-none hover:bg-chalk/[0.08] focus-visible:outline-2 focus-visible:outline-sand";

/** The current time as a column of this kind takes it (UTC, so the same everywhere). */
function nowText(kind: TemporalKind): string {
  const iso = new Date().toISOString();
  if (kind === "date") return iso.slice(0, 10);
  if (kind === "time") return iso.slice(11, 19);
  if (kind === "timestamp") return iso.slice(0, 19);
  return iso;
}

function parseField(col: RowColumn, v: Value): { ok: true; value: unknown } | { ok: false; error: string } {
  if (v.null) return { ok: true, value: null };
  if (temporalKind(col.desc) === "timestamptz" && !validTimestamptz(v.text)) {
    return { ok: false, error: "Enter a date and time with its offset, like 2026-10-01 12:00:00+00." };
  }
  const p = parseValue(col.desc, v.text);
  return p.ok ? { ok: true, value: p.value } : { ok: false, error: `${p.error[0].toUpperCase()}${p.error.slice(1)}.` };
}

export interface RowPanelHostProps {
  target: TableTarget;
  access: RowAccess;
  /** The table's entry in the object list (a view or foreign table is read-only). */
  object: ObjectSummary | undefined;
  params: EditorParams;
  navigate: (p: EditorParams) => void;
}

/**
 * The row branch of the editor's right panel: the open row (or an empty one to insert), as a full
 * panel or, collapsed, a strip. The panel stays mounted while collapsed, so a draft survives.
 */
export function RowPanelHost(props: RowPanelHostProps) {
  const panel = useTableEditor((s) => s.panel);
  const setCollapsed = useTableEditor((s) => s.setCollapsed);
  return (
    <>
      {panel.collapsed && <CollapsedStrip text={panel.row === null ? "New row" : "Row"} onExpand={() => setCollapsed(false)} />}
      <RowPanel key={`${props.target.key}|${props.target.role ?? ""}|${panel.row ?? ""}`} {...props} rowKeyText={panel.row} hidden={panel.collapsed} />
    </>
  );
}

interface RowPanelProps extends RowPanelHostProps {
  /** The open row's key (`rowKey`); null inserts a new row. */
  rowKeyText: string | null;
  hidden: boolean;
}

function RowPanel({ target, access, object, params, navigate, rowKeyText, hidden }: RowPanelProps) {
  const { databaseId, branchId, database, schema, table, role } = target;
  const qualified = `${schema}.${table}`;
  const insert = rowKeyText === null;
  const queryClient = useQueryClient();
  const openPanel = useTableEditor((s) => s.openPanel);
  const setCollapsed = useTableEditor((s) => s.setCollapsed);
  const dbParams = database ? { database } : undefined;
  const structure = useGetTable(databaseId, branchId, schema, table, dbParams);
  const types = useListTypes(databaseId, branchId, dbParams);
  const tableDef = structure.data;
  const pk = useMemo(() => (tableDef ? primaryKeyOf(tableDef) : []), [tableDef]);
  const columns = useMemo(() => (tableDef ? columnDescs(tableDef, types.data?.types ?? []) : []), [tableDef, types.data]);
  const writable = access.write && !!tableDef && pk.length > 0 && (!object || (object.kind === "table" && object.editable));
  const send = (tree: Tree) => consoleQueryExact(databaseId, branchId, { role, database, tree });
  const fail = (err: unknown, op: "select" | "insert" | "update" | "delete") => consoleErrorText(err, { role, op });

  // The open row, read by its key (a page change or a refresh elsewhere can't lose it).
  const rowQueryKey = [ROWS_QUERY, databaseId, branchId, PANEL_ROW, database, role, qualified, rowKeyText];
  const rowQuery = useQuery({
    queryKey: rowQueryKey,
    enabled: !insert && !!tableDef,
    retry: false,
    queryFn: async (): Promise<PanelRow> => {
      const values = JSON.parse(rowKeyText!) as unknown[];
      const where = whereByKey(pk, Object.fromEntries(pk.map((c, i) => [c, values[i]])));
      const read = async (cols?: string[]) =>
        (await send(buildSelect({ schema, table, page: 0, sort: null, filters: where, pk, pageSize: 1, columns: cols }))) as Row[];
      let fallback = false;
      let rows: Row[];
      try {
        rows = await read();
      } catch (err) {
        if (!isTooLarge(err)) throw err;
        fallback = true;
        rows = await read(smallColumns(tableDef!));
      }
      const row = rows.find((r) => rowKey(pk, r) === rowKeyText);
      if (!row) throw new Error("That row is no longer there (it may have been deleted, or is hidden from this role).");
      return { row, fallback };
    },
  });
  const current = rowQuery.data?.row ?? null;
  const fallback = rowQuery.data?.fallback ?? false;

  const [draft, setDraft] = useState<Record<string, Value>>({});
  const [loaded, setLoaded] = useState<Record<string, string | null>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [fieldFilter, setFieldFilter] = useState("");
  const [picking, setPicking] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const loadValue = useLoadValue(target);
  const insertDefault = useInsertDefaultRow();
  /** After a row write: the rows and the filtered count are read again. */
  const refresh = () => Promise.all(rowWriteKeys(databaseId, branchId).map((queryKey) => queryClient.invalidateQueries({ queryKey })));

  /** What a column holds before any edit. */
  function original(col: RowColumn): Value {
    if (insert) return { null: false, text: "" };
    if (col.name in loaded) return { null: loaded[col.name] === null, text: loaded[col.name] ?? "" };
    const v = current?.[col.name];
    return v === null || v === undefined ? { null: true, text: "" } : { null: false, text: editText(v) };
  }
  const valueOf = (col: RowColumn): Value => draft[col.name] ?? original(col);
  const setValue = (name: string, v: Value) => {
    setDraft((d) => ({ ...d, [name]: v }));
    setErrors(({ [name]: _gone, ...rest }) => rest);
  };
  /** Not editable here: the key of an open row, generated and identity-always columns, or everything without write access. */
  const lockedColumn = (col: RowColumn) => !writable || col.readOnly || (!insert && pk.includes(col.name));
  const unreadValue = (col: RowColumn) => !insert && fallback && !(col.name in loaded) && current !== null && !(col.name in current);
  const changed = columns.filter((c) => {
    if (lockedColumn(c) || !(c.name in draft) || unreadValue(c)) return false;
    const was = original(c);
    const now = draft[c.name];
    return now.null !== was.null || (!now.null && now.text !== was.text);
  });

  function close() {
    openPanel("closed");
  }

  async function save() {
    if (!tableDef) return;
    const next: Record<string, string> = {};
    const values: Record<string, unknown> = {};
    for (const c of insert ? columns.filter((c) => c.name in draft && !lockedColumn(c)) : changed) {
      const v = draft[c.name];
      // An emptied field on insert is left to its default, except text.
      if (insert && !v.null && v.text === "" && inputKind(c.desc) !== "text") continue;
      const p = parseField(c, v);
      if (p.ok) values[c.name] = p.value;
      else next[c.name] = p.error;
    }
    setErrors(next);
    if (Object.keys(next).length > 0) return;
    setSaving(true);
    setFailure(null);
    try {
      if (insert) {
        let row: Row | undefined;
        if (Object.keys(values).length === 0) {
          // Nothing filled: a row of defaults (an insert needs at least one value).
          const res = await insertDefault.mutateAsync({ id: databaseId, branchId, schema, table, data: { database, role } });
          row = res.row as Row;
        } else {
          const result = (await send({ v: 1, op: "insert", into: qualified, values: [values], returning: "*" })) as Row[];
          row = Array.isArray(result) ? result[0] : undefined;
        }
        await refresh();
        if (row && pk.length > 0) {
          const key = rowKey(pk, row);
          queryClient.setQueryData<PanelRow>([ROWS_QUERY, databaseId, branchId, PANEL_ROW, database, role, qualified, key], { row, fallback: false });
          openPanel("row", { row: key });
        } else {
          close();
        }
      } else {
        const where = whereByKey(pk, current!);
        const result = (await send({ v: 1, op: "update", table: qualified, set: values, where, returning: "*" })) as Row[];
        if (!Array.isArray(result) || result.length === 0) {
          setFailure("No row was changed (it may be hidden from this role, or gone).");
          return;
        }
        queryClient.setQueryData<PanelRow>(rowQueryKey, { row: result[0], fallback: false });
        setDraft({});
        setLoaded({});
        await refresh();
      }
    } catch (err) {
      setFailure(fail(err, insert ? "insert" : "update"));
    } finally {
      setSaving(false);
    }
  }

  async function confirmDelete() {
    setDeleteBusy(true);
    setDeleteError(null);
    let result: unknown;
    try {
      // The delete returns the key of the row it deleted: none means it was hidden from the role, or gone.
      result = await send({ v: 1, op: "delete", from: qualified, where: whereByKey(pk, current!), returning: pk });
    } catch (err) {
      setDeleteBusy(false);
      setDeleteError(fail(err, "delete"));
      return;
    }
    setDeleteBusy(false);
    setDeleting(false);
    const notice = deletedNotice(1, Array.isArray(result) ? result.length : null);
    if (notice) setFailure(notice);
    else close();
    await refresh();
  }

  const needle = fieldFilter.trim().toLowerCase();
  const shown = columns.length > FILTER_FIELDS_ABOVE && needle !== "" ? columns.filter((c) => c.name.toLowerCase().includes(needle)) : columns;
  const pkSummary = !insert && current ? pk.map((c) => `${c} = ${editText(current[c])}`).join(", ") : "";
  const loading = !tableDef || (!insert && rowQuery.isPending);
  const loadError = structure.error ?? rowQuery.error;
  const fkColumns = picking && tableDef ? fkOf(tableDef, picking) : null;

  function field(col: RowColumn) {
    const v = valueOf(col);
    const locked = lockedColumn(col);
    const kind = inputKind(col.desc);
    const temporal = temporalKind(col.desc);
    const fk = tableDef ? fkOf(tableDef, col.name) : null;
    const isKey = pk.includes(col.name);
    const error = errors[col.name];
    const placeholder = insert && (col.desc.has_default || col.readOnly) ? (col.readOnly && !col.desc.has_default ? "generated" : "default") : undefined;
    const isUnread = unreadValue(col);
    const nullOn = v.null;
    const set = (text: string) => setValue(col.name, { null: false, text });
    let control;
    if (isUnread) {
      control = (
        <LoadValueButton
          column={col.name}
          load={() => loadValue.load(cellKey(pk, current!), col.name)}
          onLoaded={(value) => setLoaded((l) => ({ ...l, [col.name]: value }))}
          errorText={loadValue.errorText}
        />
      );
    } else if ((kind === "bool" || kind === "enum") && !locked) {
      const options = kind === "bool" ? ["true", "false"] : (col.labels ?? []);
      const shownOptions = v.text === "" || options.includes(v.text) ? options : [v.text, ...options];
      control = (
        <div className="min-w-0 flex-1">
          <Picker
            label={col.name}
            look="field"
            mono
            disabled={nullOn}
            placeholder={nullOn ? "NULL" : placeholder}
            options={shownOptions.map((o) => ({ value: o, label: o }))}
            value={nullOn || v.text === "" ? null : v.text}
            onChange={set}
          />
        </div>
      );
    } else if (kind === "json" && !locked) {
      // The grid's small CodeMirror; NULL shows as a disabled field until it's switched off.
      control = nullOn ? (
        <input aria-label={col.name} value="" disabled placeholder="NULL" className={inputClass} />
      ) : (
        <div className="min-w-0 flex-1">
          <JsonEditor label={col.name} initial={v.text} value={v.text} invalid={!!error} onChange={set} />
        </div>
      );
    } else if (temporal && temporal !== "timestamptz" && !locked) {
      control = (
        <input
          aria-label={col.name}
          aria-invalid={error ? true : undefined}
          type={temporal === "date" ? "date" : temporal === "time" ? "time" : "datetime-local"}
          step={temporal === "date" ? undefined : 1}
          value={nullOn ? "" : toInputValue(temporal, v.text)}
          disabled={nullOn}
          onChange={(e) => set(e.target.value)}
          className={cn(inputClass, "[color-scheme:dark]")}
        />
      );
    } else if (col.desc.type === "text" && !col.desc.array && !locked) {
      control = (
        <textarea
          aria-label={col.name}
          aria-invalid={error ? true : undefined}
          value={nullOn ? "" : v.text}
          rows={3}
          spellCheck={false}
          disabled={nullOn}
          placeholder={nullOn ? "NULL" : placeholder}
          onChange={(e) => set(e.target.value)}
          className={cn(inputClass, "h-auto min-h-[4.5rem] resize-y py-2 font-sans")}
        />
      );
    } else {
      control = (
        <input
          aria-label={col.name}
          aria-invalid={error ? true : undefined}
          value={nullOn ? "" : v.text}
          readOnly={locked}
          disabled={nullOn && !locked}
          spellCheck={false}
          autoComplete="off"
          inputMode={kind === "int" || kind === "float" ? "decimal" : undefined}
          placeholder={nullOn ? "NULL" : placeholder}
          onChange={(e) => set(e.target.value)}
          className={cn(inputClass, (kind === "int" || kind === "float") && "text-right")}
        />
      );
    }
    const hasValue = !nullOn && v.text !== "";
    return (
      <div key={col.name} className="space-y-1">
        <div className="flex items-baseline gap-2 font-mono text-[13px]">
          <span>{col.name}</span>
          <span className="text-xs text-chalk-dim">
            {col.desc.type}
            {fk ? ` → ${fk.table}` : ""}
            {col.desc.nullable ? " null" : ""}
          </span>
          {isKey && !insert && <span className="ml-auto border border-line px-1.5 font-sans text-[11px] text-chalk-dim">key</span>}
        </div>
        <div className="flex items-center gap-2">
          {control}
          {!locked && !isUnread && temporal && (
            <button type="button" aria-label={`Set ${col.name} to now`} className={linkButton} onClick={() => set(nowText(temporal))}>
              Now
            </button>
          )}
          {fk && !locked && !isUnread && (
            <button type="button" aria-label={`Pick ${col.name}`} className={smallButton} onClick={() => setPicking(col.name)}>
              Pick…
            </button>
          )}
          {fk && hasValue && !insert && (
            <button
              type="button"
              aria-label={`Open ${col.name}`}
              className={linkButton}
              onClick={() => navigate({ ...params, schema: fk.schema, table: fk.table })}
            >
              Open {fk.table}
            </button>
          )}
        </div>
        {col.desc.nullable && !locked && !isUnread && (
          <label className="flex items-center gap-2 text-xs text-chalk-dim">
            <Switch aria-label={`Set ${col.name} to NULL`} checked={nullOn} onCheckedChange={(on) => setValue(col.name, on ? { null: true, text: v.text } : { null: false, text: v.text })} />
            <span aria-hidden="true">NULL</span>
          </label>
        )}
        {error && (
          <p role="alert" className="text-xs text-coral">
            {error}
          </p>
        )}
      </div>
    );
  }

  const dirtyCount = insert ? Object.keys(draft).length : changed.length;
  return (
    <>
      <aside
        role="complementary"
        aria-label={insert ? "New row" : "Row"}
        hidden={hidden}
        className="flex w-[26rem] max-w-[45vw] shrink-0 flex-col border-l border-line-strong bg-deep"
      >
        <div className="flex shrink-0 items-start justify-between gap-2 border-b border-line px-4 pt-3.5 pb-3">
          <div className="min-w-0">
            <h2 className="flex items-baseline gap-2.5 text-base font-semibold">
              {insert ? "New row" : "Row"}
              {pkSummary && <span className="truncate font-mono text-[13px] font-normal text-chalk-dim">{pkSummary}</span>}
            </h2>
            <p className="mt-1.5 truncate text-[13px] text-chalk-dim">
              {qualified} · acting as {role ?? "the owner"}
            </p>
          </div>
          <CollapseButton onClick={() => setCollapsed(true)} />
        </div>
        {columns.length > FILTER_FIELDS_ABOVE && (
          <div className="shrink-0 border-b border-line px-4 py-2">
            <input
              aria-label="Filter fields"
              placeholder="Filter fields"
              value={fieldFilter}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setFieldFilter(e.target.value)}
              className={inputClass}
            />
          </div>
        )}
        <div className="min-h-0 flex-1 space-y-3.5 overflow-y-auto px-4 py-3">
          {loadError && (
            <p role="alert" className="text-sm text-coral">
              {fail(loadError, "select")}
            </p>
          )}
          {loading && !loadError && (
            <p role="status" className="text-sm text-chalk-dim">
              {insert ? "Loading…" : "Loading row…"}
            </p>
          )}
          {!loading && !writable && <p className="text-sm text-chalk-dim">{access.write ? "This row is read-only here." : "Viewers can read rows but not change them."}</p>}
          {!loadError && (insert || current) && shown.map(field)}
          {needle !== "" && shown.length === 0 && <p className="text-sm text-chalk-dim">No field matches.</p>}
        </div>
        {failure && (
          <p role="alert" className="shrink-0 px-4 pb-2 text-sm text-coral">
            {failure}
          </p>
        )}
        <p className="shrink-0 px-4 pb-2 text-[13px] text-chalk-dim">
          {dirtyCount === 0 ? (insert ? "Untouched fields use their defaults" : "No changes") : `${dirtyCount} ${dirtyCount === 1 ? "field" : "fields"} ${insert ? "filled" : "changed"}`}
        </p>
        <div className="flex h-[60px] shrink-0 items-center gap-2 border-t border-line-strong px-4">
          {!insert && writable && (
            <Button variant="destructive" onClick={() => { setDeleteError(null); setDeleting(true); }}>
              Delete row
            </Button>
          )}
          <span className="flex-1" />
          <Button variant="ghost" onClick={close}>
            Cancel
          </Button>
          {writable && (
            <Button disabled={saving || loading || (!insert && changed.length === 0)} onClick={() => void save()}>
              {saving ? "Saving…" : "Save row"}
            </Button>
          )}
        </div>
      </aside>
      {fkColumns && (
        <FkPicker
          target={target}
          fk={fkColumns}
          onClose={() => setPicking(null)}
          onPick={(values) => {
            setDraft((d) => ({
              ...d,
              ...Object.fromEntries(Object.entries(values).map(([c, val]) => [c, val === null || val === undefined ? { null: true, text: "" } : { null: false, text: editText(val) }])),
            }));
            setPicking(null);
          }}
        />
      )}
      {deleting && (
        <Modal
          open
          onOpenChange={(open) => !open && !deleteBusy && setDeleting(false)}
          title="Delete this row?"
          description={`${pkSummary || "This row"} is deleted from ${qualified}. This can't be undone.`}
        >
          {role === null && <p className="text-sm text-chalk-dim">You act as the owner, so row-level security doesn&apos;t limit this.</p>}
          {deleteError && (
            <p role="alert" className="text-sm text-coral">
              {deleteError}
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button variant="ghost" disabled={deleteBusy} onClick={() => setDeleting(false)}>
              Cancel
            </Button>
            <Button variant="destructive" disabled={deleteBusy} onClick={() => void confirmDelete()}>
              {deleteBusy ? "Deleting…" : "Delete row"}
            </Button>
          </div>
        </Modal>
      )}
    </>
  );
}
