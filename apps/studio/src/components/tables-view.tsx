import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  ApiError,
  consoleQueryExact,
  useGetConsoleSchema,
  useGetDatabase,
  useListBranchDatabases,
  useListBranches,
  useListRoles,
  type ColumnDesc,
  type TableDesc,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import {
  buildFilterTree,
  buildSelect,
  consoleErrorText,
  pageSizeFor,
  whereByKey,
  type ConsoleOp,
  type FilterSet,
  type Sort,
  type Tree,
} from "@/lib/console";
import { errorText } from "@/lib/errors";
import { ConsoleFilters } from "./console-filters";
import { ConsoleGrid, rowKey, type Row } from "./console-grid";
import { OwnerPasswordNotice } from "./owner-password-notice";
import { InsertForm } from "./row-editor";

export interface TablesViewProps {
  databaseId: string;
}

const field = "h-8 rounded-lg border border-input bg-background px-2 text-sm";
const READ_ONLY_KIND: Record<string, string> = {
  view: "Views are read-only.",
  materialized_view: "Materialized views are read-only.",
  foreign_table: "Foreign tables are read-only here.",
};
const NO_PK = "This table has no primary key, so the console can't change its rows safely. It is read-only.";

/** Browse and edit rows of any served database, as the owner or as a plain role. */
export function TablesView({ databaseId }: TablesViewProps) {
  const branches = useListBranches(databaseId);
  const detail = useGetDatabase(databaseId);
  const pageSize = pageSizeFor(detail.data?.max_rows);
  const eligible = (branches.data ?? []).filter((b) => b.kind === "normal" && b.status === "ready");
  const [pickedBranch, setPickedBranch] = useState<string | null>(null);
  const branchId = pickedBranch ?? (eligible.find((b) => b.parent_branch_id === null) ?? eligible[0])?.id ?? null;

  const dbs = useListBranchDatabases(databaseId, branchId ?? "", { query: { enabled: branchId !== null } });
  const served = (dbs.data ?? []).filter((d) => d.data_api === "on");
  const [pickedDb, setPickedDb] = useState<string | null>(null);
  const db = served.find((d) => d.name === pickedDb) ?? served.find((d) => d.main) ?? served[0] ?? null;
  const dbParam = db && !db.main ? db.name : null;

  const roles = useListRoles(databaseId, branchId ?? "", { query: { enabled: branchId !== null } });
  const [role, setRole] = useState("");

  // What the viewing role reaches (the owner without a role), so the lists match what it can read.
  const schemaParams = { ...(dbParam ? { database: dbParam } : {}), ...(role !== "" ? { role } : {}) };
  const schemaQuery = useGetConsoleSchema(databaseId, branchId ?? "", schemaParams, {
    query: { enabled: branchId !== null && db !== null, retry: false },
  });
  const schema = schemaQuery.data;
  const [pickedSchema, setPickedSchema] = useState<string | null>(null);
  const schemaNames = Object.keys(schema?.schemas ?? {});
  const schemaName = pickedSchema && schemaNames.includes(pickedSchema) ? pickedSchema : (schema?.default_schema ?? schemaNames[0] ?? null);
  const entry = schemaName ? schema?.schemas[schemaName] : undefined;
  const [table, setTable] = useState<string | null>(null);
  const desc = entry && table ? entry.tables[table] : undefined;

  const error = branches.error ?? dbs.error ?? roles.error ?? schemaQuery.error;
  const errorMessage = error ? consoleErrorText(error, { role: role === "" ? null : role, op: "select" }) : null;
  const roleRows = (roles.data ?? []).filter((r) => r.origin !== "owner" && r.name !== "anon" && r.name !== "authenticated");

  return (
    <div className="space-y-4 py-3">
      <div className="flex flex-wrap items-end gap-3 text-sm">
        <label className="flex flex-col gap-1">
          <span>Branch</span>
          <select
            value={branchId ?? ""}
            onChange={(e) => {
              setPickedBranch(e.target.value);
              setPickedDb(null);
              setPickedSchema(null);
              setTable(null);
              setRole("");
            }}
            className={field}
          >
            {eligible.map((b) => (
              <option key={b.id} value={b.id}>
                {b.name}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1">
          <span>Database</span>
          <select
            value={db?.name ?? ""}
            onChange={(e) => {
              setPickedDb(e.target.value);
              setPickedSchema(null);
              setTable(null);
            }}
            className={field}
          >
            {served.map((d) => (
              <option key={d.name}>{d.name}</option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1">
          <span>Schema</span>
          <select
            value={schemaName ?? ""}
            onChange={(e) => {
              setPickedSchema(e.target.value);
              setTable(null);
            }}
            className={field}
          >
            {schemaNames.map((s) => (
              <option key={s}>{s}</option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1">
          <span>View as</span>
          <select value={role} onChange={(e) => setRole(e.target.value)} className={field}>
            <option value="">Owner</option>
            <option value="anon">anon</option>
            <option value="authenticated">authenticated</option>
            {roleRows.map((r) => (
              <option key={r.name} value={r.name} disabled={r.level === "owner_level"}>
                {r.level === "owner_level" ? `${r.name} (can bypass row-level security, so it can't be used)` : r.name}
              </option>
            ))}
          </select>
        </label>
      </div>

      {role === "" && (
        <p className="rounded-lg border border-destructive/40 p-2 text-sm">
          Viewing as owner: row-level security is bypassed, and edits apply to every row.
        </p>
      )}
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error instanceof ApiError ? errorMessage : errorText(error)}
        </p>
      )}
      {branchId !== null && dbs.data !== undefined && served.length === 0 && (
        <p className="text-sm text-muted-foreground">No database of this branch is served by the data API yet.</p>
      )}

      {entry && schemaName && (
        <div className="grid gap-4 md:grid-cols-[14rem_1fr]">
          <nav aria-label="Objects" className="space-y-1 text-sm">
            <h3 className="font-medium">Tables and views</h3>
            {Object.entries(entry.tables).map(([name, t]) => (
              <button
                key={name}
                type="button"
                aria-pressed={table === name}
                aria-label={`${name}${t.kind !== "table" ? `, ${t.kind.replace("_", " ")}` : ""}${t.rls ? ", row-level security on" : ""}`}
                onClick={() => setTable(name)}
                className={`flex w-full items-center gap-2 rounded px-2 py-1 text-left hover:bg-muted ${table === name ? "bg-muted" : ""}`}
              >
                {name}
                {t.kind !== "table" && <span className="text-xs text-muted-foreground">{t.kind.replace("_", " ")}</span>}
                {t.rls && <span className="rounded-full border px-1.5 text-xs">RLS</span>}
              </button>
            ))}
            {Object.keys(entry.functions).length > 0 && (
              <>
                <h3 className="pt-2 font-medium">Functions</h3>
                <ul className="px-2 text-muted-foreground">
                  {Object.keys(entry.functions).map((f) => (
                    <li key={f}>{f}()</li>
                  ))}
                </ul>
              </>
            )}
          </nav>
          <div>
            {desc && table && branchId ? (
              <TableData
                key={`${branchId}|${dbParam}|${role}|${schemaName}.${table}`}
                databaseId={databaseId}
                branchId={branchId}
                database={dbParam}
                role={role === "" ? null : role}
                schemaName={schemaName}
                tableName={table}
                desc={desc}
                enums={entry.enums}
                pageSize={pageSize}
              />
            ) : (
              <p className="text-sm text-muted-foreground">Pick a table or view.</p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

interface TableDataProps {
  databaseId: string;
  branchId: string;
  database: string | null;
  role: string | null;
  schemaName: string;
  tableName: string;
  desc: TableDesc;
  enums: Record<string, string[]>;
  pageSize: number;
}

function TableData({ databaseId, branchId, database, role, schemaName, tableName, desc, enums, pageSize }: TableDataProps) {
  const columns: Record<string, ColumnDesc> = desc.columns;
  const pk = desc.primary_key ?? [];
  const readOnlyReason = READ_ONLY_KIND[desc.kind] ?? (pk.length === 0 ? NO_PK : null);
  const readOnly = readOnlyReason !== null;

  const [page, setPage] = useState(0);
  const [sort, setSort] = useState<Sort | null>(null);
  const [filters, setFilters] = useState<FilterSet>({ mode: "and", rows: [] });
  const [where, setWhere] = useState<Tree | null>(null);
  const [filterError, setFilterError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [inserting, setInserting] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const tree = buildSelect({ schema: schemaName, table: tableName, page, sort, filters: where, pk, pageSize });
  const send = (t: Tree) => consoleQueryExact(databaseId, branchId, { role, database, tree: t });
  const rowsQuery = useQuery({
    queryKey: ["console", databaseId, branchId, database, role, tree],
    queryFn: () => send(tree) as Promise<Row[]>,
    retry: false,
    gcTime: 0,
  });
  const rows = rowsQuery.data ?? [];
  const refetch = () => void rowsQuery.refetch();
  const failure = rowsQuery.error;
  const ownerUnknown = failure instanceof ApiError && failure.code === "owner_password_unknown";
  const text = (err: unknown, op: ConsoleOp) => consoleErrorText(err, { role, op });

  /** Runs a write; returns an error text, or null on success. */
  async function write(t: Tree, op: ConsoleOp, expectRow = false): Promise<string | null> {
    setBusy(true);
    try {
      const result = await send(t);
      if (expectRow && Array.isArray(result) && result.length === 0) {
        refetch();
        return "No row was changed (it may be hidden from this role, or gone).";
      }
      refetch();
      return null;
    } catch (err) {
      return err instanceof ApiError && err.code === "owner_password_unknown"
        ? "The owner password isn't known; reset it first (see the notice above)."
        : text(err, op);
    } finally {
      setBusy(false);
    }
  }

  function onSort(col: string) {
    setPage(0);
    setSelected(new Set());
    setSort(sort?.col !== col ? { col, dir: "asc" } : sort.dir === "asc" ? { col, dir: "desc" } : null);
  }
  function apply() {
    const built = buildFilterTree(filters, columns);
    if (!built.ok) return setFilterError(built.error);
    setFilterError(null);
    setPage(0);
    setSelected(new Set());
    setWhere(built.where ?? null);
  }
  function goto(next: number) {
    setPage(next);
    setSelected(new Set());
    setConfirmDelete(false);
  }

  async function removeSelected() {
    let ops;
    try {
      ops = rows
        .filter((r) => selected.has(rowKey(pk, r)))
        .map((r) => ({ op: "delete", from: `${schemaName}.${tableName}`, where: whereByKey(pk, r) }));
    } catch (err) {
      return setDeleteError(err instanceof Error ? err.message : "Something went wrong");
    }
    const wholePage = ops.length === rows.length;
    const failed = await write({ v: 1, op: "batch", ops }, "delete");
    if (failed) return setDeleteError(failed);
    if (wholePage && page > 0) setPage(page - 1);
    setSelected(new Set());
    setConfirmDelete(false);
    setDeleteError(null);
  }

  return (
    <div className="space-y-3">
      <h2 className="font-medium">
        {schemaName}.{tableName}
      </h2>
      {readOnlyReason && <p className="text-sm text-muted-foreground">{readOnlyReason}</p>}
      <ConsoleFilters columns={columns} value={filters} onChange={setFilters} onApply={apply} error={filterError} />

      {ownerUnknown && <OwnerPasswordNotice databaseId={databaseId} branchId={branchId} onRetry={refetch} />}
      {failure && !ownerUnknown && (
        <p role="alert" className="text-sm text-destructive">
          {text(failure, "select")}
        </p>
      )}

      {!readOnly && (
        <div className="flex flex-wrap items-center gap-2">
          <Button size="sm" variant="outline" onClick={() => setInserting(true)}>
            Insert row
          </Button>
          <Button
            size="sm"
            variant="outline"
            disabled={selected.size === 0 || busy}
            onClick={() => {
              setConfirmDelete(true);
              setDeleteError(null);
            }}
          >
            Delete selected ({selected.size})
          </Button>
        </div>
      )}
      {inserting && (
        <InsertForm
          columns={columns}
          enums={enums}
          onCancel={() => setInserting(false)}
          onInsert={async (values) => {
            const failed = await write({ v: 1, op: "insert", into: `${schemaName}.${tableName}`, values: [values], returning: pk }, "insert", true);
            if (!failed) setInserting(false);
            return failed;
          }}
        />
      )}
      {confirmDelete && (
        <div className="space-y-2 rounded-lg border p-3 text-sm">
          <p>
            Delete {selected.size} {selected.size === 1 ? "row" : "rows"}? This can&apos;t be undone.
            {role === null && " You are the owner, so row-level security does not limit this."}
          </p>
          {deleteError && (
            <p role="alert" className="text-destructive">
              {deleteError}
            </p>
          )}
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void removeSelected()}>
              Confirm delete
            </Button>
            <Button size="sm" variant="outline" onClick={() => setConfirmDelete(false)}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {rowsQuery.data === undefined && !failure ? (
        <p className="text-sm text-muted-foreground">Loading rows…</p>
      ) : (
        rowsQuery.data !== undefined && (
          <ConsoleGrid
            columns={columns}
            enums={enums}
            rows={rows}
            pk={pk}
            readOnly={readOnly}
            sort={sort}
            onSort={onSort}
            selected={selected}
            onToggle={(key) => {
              const next = new Set(selected);
              if (!next.delete(key)) next.add(key);
              setSelected(next);
            }}
            onSave={(row, col, value) =>
              write({ v: 1, op: "update", table: `${schemaName}.${tableName}`, set: { [col]: value }, where: whereByKey(pk, row), returning: pk }, "update", true)
            }
          />
        )
      )}

      <div className="flex items-center gap-2 text-sm">
        <Button size="sm" variant="outline" aria-label="Previous page" disabled={page === 0} onClick={() => goto(page - 1)}>
          Previous
        </Button>
        <span>Page {page + 1}</span>
        <Button size="sm" variant="outline" aria-label="Next page" disabled={rows.length < pageSize} onClick={() => goto(page + 1)}>
          Next
        </Button>
      </div>
    </div>
  );
}
