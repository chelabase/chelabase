import { useEffect, useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { consoleQueryExact, useGetTable, useInsertDefaultRow, useListTypes, type ObjectSummary } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { Switch } from "@/components/ui/switch";
import { buildSelect, consoleErrorText, inputKind, parseValue, type Tree } from "@/lib/console";
import { NO_CHECK_NOTICE, conflictWhere, isComparable, splitTextRows, type Originals } from "@/lib/realtime/conflict";
import { insertedKey, textSelectItems, whereByTextKey } from "@/lib/realtime/keys";
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
import { useTableEditor } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { JsonEditor, toInputValue } from "./cell-editor";
import { ConflictDialog, type ConflictDialogProps } from "./conflict-dialog";
import { FkPicker, fkOf } from "./fk-picker";
import { LoadValueButton, cellKey, useLoadValue } from "./load-value";
import { Picker } from "./picker";
import { CollapseButton, CollapsedStrip } from "./panel-bits";
import { deletedNotice, panelRowKey, rowWriteKeys, seedPanelRow } from "./rows-view";
import type { TableTarget } from "./target";

/** Fields above this many get a "Filter fields" input. */
export const FILTER_FIELDS_ABOVE = 30;

/** Numbers the mounted row panels (their `RowDraft.id`). */
let draftIds = 0;

const NO_MARKS: ReadonlySet<string> = new Set();

/** A read value as the panel compares it across reads: its edit text, null for NULL. */
const readText = (v: unknown): string | null => (v === null || v === undefined ? null : editText(v));

/** What a field holds while it's edited: NULL, or text the column's type parses. */
interface Value {
  null: boolean;
  text: string;
}

interface PanelRow {
  row: Row;
  /** The row was too large: its large columns weren't read ("Load value"). */
  fallback: boolean;
  /** The comparable columns as text, as read; absent when the row came from a write's `returning`. */
  originals?: Originals;
}

/** A save that matched no row (§6.3). */
interface PanelConflict {
  state: ConflictDialogProps["state"];
  values: Record<string, unknown>;
  /** The row as re-read (Take theirs shows it). */
  fresh: PanelRow | null;
  unchecked: string[];
  busy: boolean;
  error: string | null;
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

/** A re-read row's value as the conflict dialog shows it: its text as read, else its JSON text; null for NULL. */
function theirText(fresh: PanelRow, column: string): string | null {
  if (fresh.originals && Object.hasOwn(fresh.originals, column)) return fresh.originals[column];
  const v = fresh.row[column];
  return v === null || v === undefined ? null : editText(v);
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
  // A Discard remounts the panel empty.
  const epoch = useTableEditor((s) => s.rowDraftEpoch);
  return (
    <>
      {panel.collapsed && <CollapsedStrip text={panel.row === null ? "New row" : "Row"} onExpand={() => setCollapsed(false)} />}
      <RowPanel key={`${props.target.key}|${props.target.role ?? ""}|${panel.row ?? ""}|${epoch}`} {...props} rowKeyText={panel.row} hidden={panel.collapsed} />
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
  const structure = useGetTable(databaseId, branchId, schema, table, dbParams, { query: { enabled: !target.notTable } });
  const types = useListTypes(databaseId, branchId, dbParams);
  const tableDef = structure.data;
  const pk = useMemo(() => (tableDef ? primaryKeyOf(tableDef) : []), [tableDef]);
  const columns = useMemo(() => (tableDef ? columnDescs(tableDef, types.data?.types ?? []) : []), [tableDef, types.data]);
  const writable = access.write && !!tableDef && pk.length > 0 && (!object || (object.kind === "table" && object.editable));
  const send = (tree: Tree) => consoleQueryExact(databaseId, branchId, { role, database, tree });
  const fail = (err: unknown, op: "select" | "insert" | "update" | "delete") => consoleErrorText(err, { role, op });

  // The comparable columns (R18): read as text with the row, the conflict check's originals.
  const comparable = useMemo(() => {
    const kinds = new Map((types.data?.types ?? []).map((t) => [t.oid, t.kind]));
    const composite = new Set((tableDef?.columns ?? []).filter((c) => kinds.get(c.type_oid) === "composite").map((c) => c.name));
    return columns.filter((c) => !pk.includes(c.name) && !composite.has(c.name) && isComparable(c.desc)).map((c) => c.name);
  }, [columns, pk, tableDef, types.data]);

  /** The row by its key, with its key and originals as text; null when no row has that key for this role. */
  async function readRow(key: string): Promise<PanelRow | null> {
    const where = whereByTextKey(pk, key);
    const read = async (cols: string[]) => {
      const sel = textSelectItems({ columns: cols, pk, originals: comparable.filter((c) => cols.includes(c)) });
      const raw = (await send(buildSelect({ schema, table, page: 0, sort: null, filters: where, pk, pageSize: 1, columns: sel.items }))) as Row[];
      return splitTextRows(raw, sel);
    };
    let fallback = false;
    let got;
    try {
      got = await read(columns.map((c) => c.name));
    } catch (err) {
      if (!isTooLarge(err)) throw err;
      fallback = true;
      got = await read(smallColumns(tableDef!));
    }
    const i = got.keys.indexOf(key);
    return i < 0 ? null : { row: got.rows[i], fallback, originals: got.originals[i] };
  }

  // The open row, read by its key (a page change or a refresh elsewhere can't lose it).
  const rowQueryKey = panelRowKey(target, rowKeyText);
  const rowQuery = useQuery({
    queryKey: rowQueryKey,
    enabled: !insert && !!tableDef && !types.isPending,
    retry: false,
    queryFn: async (): Promise<PanelRow> => {
      const found = await readRow(rowKeyText!);
      if (!found) throw new Error("That row is no longer there (it may have been deleted, or is hidden from this role).");
      return found;
    },
  });
  const current = rowQuery.data?.row ?? null;
  const fallback = rowQuery.data?.fallback ?? false;
  // W1: the originals as the panel first read the row; later reads (a live change, a refresh) never replace them.
  const [captured, setCaptured] = useState<Originals | null>(null);
  if (captured === null && rowQuery.data?.originals) setCaptured(rowQuery.data.originals);
  const [conflict, setConflict] = useState<PanelConflict | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const [draft, setDraft] = useState<Record<string, Value>>({});
  const [loaded, setLoaded] = useState<Record<string, string | null>>({});
  // A later read of the row (a live change): the fields the viewer hasn't touched show its values, so their originals
  // are what the viewer now sees; a touched field keeps the original it was edited from (W1).
  const [seenData, setSeenData] = useState(rowQuery.data);
  if (rowQuery.data !== seenData) {
    setSeenData(rowQuery.data);
    const fresh = rowQuery.data?.originals;
    if (captured !== null && fresh) {
      const next = { ...captured };
      for (const [c, v] of Object.entries(fresh)) if (!(c in draft)) next[c] = v;
      setCaptured(next);
    }
  }
  // The live cue, only while the panel holds unsaved edits: untouched fields a live update changed ("Updated"), and
  // edited fields whose value changed on the server since ("Changed since you started"; the save's conflict check decides).
  const [liveMarks, setLiveMarks] = useState<ReadonlySet<string>>(NO_MARKS);
  const [sinceMarks, setSinceMarks] = useState<ReadonlySet<string>>(NO_MARKS);
  const unmarkLive = (names: string[]) =>
    setLiveMarks((m) => (names.some((n) => m.has(n)) ? new Set([...m].filter((n) => !names.includes(n))) : m));
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
    unmarkLive([name]);
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

  /** Cancel: asks first while the draft holds unsaved edits. */
  function close() {
    openPanel("closed");
  }
  /** After a confirmed delete (or an insert whose row can't be shown): nothing is left to discard. */
  function closeDone() {
    openPanel("closed", undefined, { discard: true });
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
    setNotice(null);
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
        // The panel reads the new row by its text key (read back when the returned values aren't their texts).
        const key = row ? await insertedKey({ schema, table, pk, row, types: Object.fromEntries(columns.map((c) => [c.name, c.desc])), send }).catch(() => null) : null;
        if (key !== null && row) {
          seedPanelRow(queryClient, target, key, row);
          // The draft is saved: the saved row opens without asking.
          openPanel("row", { row: key }, { discard: true });
        }
        else closeDone();
      } else {
        // Never without originals (review I4): after a save the row is read again first.
        if (captured === null) return;
        // W1: each edited column's original as the panel first read it (in the order they were edited).
        const edited = Object.keys(draft).filter((c) => c in values);
        const check = conflictWhere({ pk, key: rowKeyText!, edited, originals: captured });
        const result = (await send({ v: 1, op: "update", table: qualified, set: values, where: check.where, returning: "*" })) as Row[];
        if (!Array.isArray(result)) {
          setFailure("No row was changed (it may be hidden from this role, or gone).");
          return;
        }
        if (result.length === 0) {
          // Changed since the panel opened, or gone for this role: read it again.
          const fresh = await readRow(rowKeyText!);
          setConflict({
            values,
            fresh,
            unchecked: check.unchecked,
            busy: false,
            error: null,
            state: fresh
              ? {
                  kind: "changed",
                  columns: edited.map((name) => ({ name, theirs: theirText(fresh, name), yours: draft[name]?.null ? null : (draft[name]?.text ?? null) })),
                }
              : { kind: "gone" },
          });
          return;
        }
        saved(result);
        if (check.unchecked.length > 0) setNotice(`${NO_CHECK_NOTICE}: ${check.unchecked.join(", ")}.`);
        await refresh();
      }
    } catch (err) {
      setFailure(fail(err, insert ? "insert" : "update"));
    } finally {
      setSaving(false);
    }
  }

  /** A saved update: the panel shows the returned row; its originals are captured again from the refresh's read. */
  function saved(result: Row[]) {
    queryClient.setQueryData<PanelRow>(rowQueryKey, { row: result[0], fallback: false });
    setCaptured(null);
    setDraft({});
    setLoaded({});
  }

  async function overwrite() {
    if (!conflict) return;
    setConflict({ ...conflict, busy: true, error: null });
    try {
      const result = (await send({ v: 1, op: "update", table: qualified, set: conflict.values, where: whereByTextKey(pk, rowKeyText!), returning: "*" })) as Row[];
      if (!Array.isArray(result) || result.length === 0) {
        setConflict((c) => (c ? { ...c, busy: false, state: { kind: "gone" } } : c));
        return;
      }
      setConflict(null);
      saved(result);
      await refresh();
    } catch (err) {
      setConflict((c) => (c ? { ...c, busy: false, error: fail(err, "update") } : c));
    }
  }

  function takeTheirs() {
    const fresh = conflict?.fresh;
    if (fresh) {
      queryClient.setQueryData<PanelRow>(rowQueryKey, fresh);
      setCaptured(fresh.originals ?? null);
    }
    setDraft({});
    setLoaded({});
    setConflict(null);
    void refresh();
  }

  async function confirmDelete() {
    setDeleteBusy(true);
    setDeleteError(null);
    let result: unknown;
    try {
      // The delete returns the key of the row it deleted: none means it was hidden from the role, or gone.
      result = await send({ v: 1, op: "delete", from: qualified, where: whereByTextKey(pk, rowKeyText!), returning: pk });
    } catch (err) {
      setDeleteBusy(false);
      setDeleteError(fail(err, "delete"));
      return;
    }
    setDeleteBusy(false);
    setDeleting(false);
    const notice = deletedNotice(1, Array.isArray(result) ? result.length : null);
    if (notice) setFailure(notice);
    else closeDone();
    await refresh();
  }

  const needle = fieldFilter.trim().toLowerCase();
  const shown = columns.length > FILTER_FIELDS_ABOVE && needle !== "" ? columns.filter((c) => c.name.toLowerCase().includes(needle)) : columns;
  const pkSummary = !insert && current ? pk.map((c) => `${c} = ${editText(current[c])}`).join(", ") : "";
  const loading = !tableDef || (!insert && rowQuery.isPending);
  // A saved row's originals are re-read before the next save can check against them (review I4).
  const awaitingOriginals = !insert && !loading && captured === null;
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
    const edited = col.name in draft;
    const cue = edited ? (sinceMarks.has(col.name) ? "since-started" : undefined) : liveMarks.has(col.name) ? "live" : undefined;
    const serverNow = cue === "since-started" ? readText(current?.[col.name]) : null;
    return (
      <div key={col.name} data-field={col.name} data-changed={cue} className="space-y-1">
        <div className="flex items-baseline gap-2 font-mono text-[13px]">
          <span>{col.name}</span>
          <span className="text-xs text-chalk-dim">
            {col.desc.type}
            {fk ? ` → ${fk.table}` : ""}
            {col.desc.nullable ? " null" : ""}
          </span>
          {cue === "live" && <span className="ml-auto font-sans text-[11px] text-seafoam">Updated</span>}
          {isKey && !insert && <span className={cn("border border-line px-1.5 font-sans text-[11px] text-chalk-dim", cue !== "live" && "ml-auto")}>key</span>}
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
        {cue === "since-started" && (
          <p className="flex flex-wrap items-baseline gap-x-2 text-xs">
            <span className="border border-seafoam/60 px-1.5 text-[11px] text-seafoam">Changed since you started</span>
            <span className="min-w-0 text-chalk-dim">
              Now: <span className={cn("font-mono break-all whitespace-pre-wrap", serverNow === null ? "text-chalk-dim" : "text-chalk")}>{serverNow ?? "NULL"}</span>
            </span>
          </p>
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
  // F6: an unsaved draft is published, so anything that would drop it asks first (and the page asks before unloading).
  const [draftId] = useState(() => ++draftIds);
  const setRowDraft = useTableEditor((s) => s.setRowDraft);
  const clearRowDraft = useTableEditor((s) => s.clearRowDraft);
  const dirty = dirtyCount > 0;
  // A new read of the row: while dirty, mark the fields whose value it changed. A save (its returned row), a
  // "Take theirs" or a discard leaves the panel clean, which drops every mark.
  const [cueSeen, setCueSeen] = useState(rowQuery.data);
  if (rowQuery.data !== cueSeen) {
    setCueSeen(rowQuery.data);
    const before = cueSeen?.row;
    const after = rowQuery.data?.row;
    if (dirty && before && after) {
      const live = new Set(liveMarks);
      const since = new Set(sinceMarks);
      for (const c of columns) {
        if (!(c.name in before) || !(c.name in after) || readText(before[c.name]) === readText(after[c.name])) continue;
        if (c.name in draft) since.add(c.name);
        else live.add(c.name);
      }
      if (live.size !== liveMarks.size) setLiveMarks(live);
      if (since.size !== sinceMarks.size) setSinceMarks(since);
    }
  }
  if (!dirty && (liveMarks.size > 0 || sinceMarks.size > 0)) {
    setLiveMarks(NO_MARKS);
    setSinceMarks(NO_MARKS);
  }
  useEffect(() => {
    if (dirty) setRowDraft({ id: draftId, table: target.key, row: rowKeyText });
    else clearRowDraft(draftId);
  }, [dirty, draftId, target.key, rowKeyText, setRowDraft, clearRowDraft]);
  useEffect(() => () => clearRowDraft(draftId), [draftId, clearRowDraft]);
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
        {notice && (
          <p role="status" className="shrink-0 px-4 pb-2 text-[13px] text-chalk-dim">
            {notice}
          </p>
        )}
        <p className="shrink-0 px-4 pb-2 text-[13px] text-chalk-dim">
          {awaitingOriginals && writable && !loadError ? "Reading the saved row again… · " : ""}
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
            <Button disabled={saving || loading || awaitingOriginals || (!insert && changed.length === 0)} onClick={() => void save()}>
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
            unmarkLive(Object.keys(values));
            setDraft((d) => ({
              ...d,
              ...Object.fromEntries(Object.entries(values).map(([c, val]) => [c, val === null || val === undefined ? { null: true, text: "" } : { null: false, text: editText(val) }])),
            }));
            setPicking(null);
          }}
        />
      )}
      {conflict && (
        <ConflictDialog
          open
          state={conflict.state}
          unchecked={conflict.unchecked}
          busy={conflict.busy}
          error={conflict.error}
          onOverwrite={() => void overwrite()}
          onTakeTheirs={takeTheirs}
          onClose={() => setConflict(null)}
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
