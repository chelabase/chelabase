import { lazy, Suspense, useRef, type KeyboardEvent, type PointerEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { getGetTableQueryKey, getListObjectsQueryKey, getListTypesQueryKey } from "@chelabase/management-client";
import type { PglsClient } from "@/lib/pgls/client";
import { SQL_HEIGHT, useTableEditor } from "@/lib/table-editor/store";
import { rowWriteKeys } from "./rows-view";

// The editor, CodeMirror and the pgls worker load only when the drawer first opens.
const SqlEditor = lazy(() => import("@/components/sql-editor/sql-editor").then((m) => ({ default: m.SqlEditor })));

export interface SqlDrawerProps {
  databaseId: string;
  branchId: string;
  branchName: string;
  branchProtected: boolean;
  /** The Postgres database's name and whether it is the branch's main one (addressed without a name). */
  pg: { name: string; main: boolean };
  pgDatabases: string[];
  /** The Act as role; null is the owner. */
  role: string | null;
  allowed: { owner: boolean; roles: string[] };
  rolesReady: boolean;
  onPgDatabase: (name: string) => void;
  onRole: (role: string) => void;
  /** The open table, refreshed after a run that changes the schema; null on the table list. */
  table: { schema: string; table: string } | null;
  onFullEditor: () => void;
  onClose: () => void;
  createClient?: () => Promise<PglsClient>;
}

const STEP = 0.05;

/**
 * The SQL drawer under the table: a resize handle over the compact SQL editor. Its height is a share of the editor's
 * column (the store's `sqlHeight`); resizing only restyles it, so the code editor (and its undo history) stays mounted.
 */
export function SqlDrawer({ databaseId, branchId, branchName, branchProtected, pg, table, onFullEditor, onClose, createClient, ...run }: SqlDrawerProps) {
  const height = useTableEditor((s) => s.sqlHeight);
  const setHeight = useTableEditor((s) => s.setSqlHeight);
  const section = useRef<HTMLElement>(null);
  const queryClient = useQueryClient();
  const params = pg.main ? undefined : { database: pg.name };

  // Realtime sends no DDL: after a run that changed the schema, the open table's structure, the object list, the types
  // and the rows (a column added or dropped) are read again. Rows written by plain DML arrive live.
  const refresh = () => {
    const keys = [
      getListObjectsQueryKey(databaseId, branchId, params),
      getListTypesQueryKey(databaseId, branchId, params),
      ...(table ? [getGetTableQueryKey(databaseId, branchId, table.schema, table.table, params)] : []),
      ...rowWriteKeys(databaseId, branchId),
    ];
    for (const queryKey of keys) void queryClient.invalidateQueries({ queryKey });
  };

  const dragTo = (clientY: number) => {
    const box = section.current?.parentElement?.getBoundingClientRect();
    if (box && box.height > 0) setHeight((box.bottom - clientY) / box.height);
  };
  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    e.preventDefault();
    e.currentTarget.setPointerCapture?.(e.pointerId);
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    if (e.currentTarget.hasPointerCapture?.(e.pointerId)) dragTo(e.clientY);
  };
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const next = { ArrowUp: height + STEP, ArrowDown: height - STEP, Home: SQL_HEIGHT.max, End: SQL_HEIGHT.min }[e.key];
    if (next === undefined) return;
    e.preventDefault();
    setHeight(next);
  };

  return (
    <section ref={section} aria-label="SQL" style={{ flex: `0 0 ${height * 100}%` }} className="flex min-h-0 flex-col border-t border-line bg-deep">
      <div
        role="separator"
        aria-label="Resize SQL"
        aria-orientation="horizontal"
        aria-valuemin={SQL_HEIGHT.min * 100}
        aria-valuemax={SQL_HEIGHT.max * 100}
        aria-valuenow={Math.round(height * 100)}
        tabIndex={0}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onKeyDown={onKeyDown}
        className="h-1.5 shrink-0 cursor-row-resize touch-none outline-none hover:bg-sand/40 focus-visible:bg-sand"
      />
      <Suspense fallback={<p className="p-3 text-sm text-chalk-dim">Loading the editor…</p>}>
        <SqlEditor
          key={databaseId}
          databaseId={databaseId}
          branchId={branchId}
          pgDatabase={pg.main ? null : pg.name}
          branchProtected={branchProtected}
          {...run}
          onSchemaChange={refresh}
          drawer={{ place: `on ${branchName} · ${pg.name} · as ${run.role ?? "owner"}`, onFullEditor, onClose }}
          createClient={createClient}
        />
      </Suspense>
    </section>
  );
}
