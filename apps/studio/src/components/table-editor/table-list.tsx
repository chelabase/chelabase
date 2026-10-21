import { useId, useState } from "react";
import type { ObjectSummary } from "@chelabase/management-client";
import { cn } from "@/lib/utils";
import { schemasOf, type EditorParams } from "@/lib/table-editor/context";
import { useTableEditor } from "@/lib/table-editor/store";
import { Picker } from "./picker";

export interface TableListProps {
  params: EditorParams;
  navigate: (p: EditorParams) => void;
  /** Absent until the branch's objects have loaded. */
  objects?: ObjectSummary[];
  /** The branch is ready but has no Postgres database: nothing is loading, and Types and New table have nowhere to go. */
  noDatabase?: boolean;
  /** The open table's exact row count when its estimate is missing (a short page told it), else null. */
  exactRows?: number | null;
}

/** "3.1k", "131k", "1.2M": a row estimate in a few characters. */
export function formatCount(n: number): string {
  if (n < 1000) return String(n);
  const round = (v: number) => (v < 100 ? Math.round(v * 10) / 10 : Math.round(v));
  // A value that rounds up to 1000k is shown in the next unit: 999 500 is "1M", not "1000k".
  const k = round(n / 1000);
  return k < 1000 ? `${k}k` : `${round(n / 1_000_000)}M`;
}

/** The count line's kinds, in its order; any other kind (a table, a partitioned table) counts as a table. */
const COUNTED: readonly { kind: string; one: string; many: string }[] = [
  { kind: "table", one: "table", many: "tables" },
  { kind: "view", one: "view", many: "views" },
  { kind: "materialized_view", one: "materialized view", many: "materialized views" },
  { kind: "foreign_table", one: "foreign table", many: "foreign tables" },
];

/** "2 tables, 1 view, 1 materialized view": each kind apart, only the kinds present; "0 tables" when empty. */
function countLine(objects: ObjectSummary[]): string {
  const counts = new Map<string, number>();
  for (const o of objects) {
    const kind = COUNTED.some((c) => c.kind === o.kind) ? o.kind : "table";
    counts.set(kind, (counts.get(kind) ?? 0) + 1);
  }
  const parts = COUNTED.flatMap(({ kind, one, many }) => {
    const n = counts.get(kind) ?? 0;
    return n > 0 ? [`${n} ${n === 1 ? one : many}`] : [];
  });
  return parts.length > 0 ? parts.join(", ") : "0 tables";
}

const ghost =
  "flex h-9 w-full items-center border border-line-strong px-3 text-left text-sm outline-none hover:bg-chalk/[0.08] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand disabled:cursor-default disabled:text-chalk-dim disabled:opacity-60 disabled:hover:bg-transparent";

const NO_DATABASE = "This branch has no Postgres database";

/** The left list: the schema, a filter, the schema's tables and views, and the entries to Types and a new table. */
export function TableList({ params, navigate, objects, noDatabase = false, exactRows = null }: TableListProps) {
  const [filter, setFilter] = useState("");
  const schemas = objects ? schemasOf(objects) : [];
  const schema = params.schema ?? null;
  const needle = filter.trim().toLowerCase();
  const shown = (objects ?? [])
    .filter((o) => o.schema === schema && o.name.toLowerCase().includes(needle))
    .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  const inSchema = (objects ?? []).filter((o) => o.schema === schema);
  const ids = useId();
  const view = useTableEditor((s) => s.view);
  const setView = useTableEditor((s) => s.setView);
  const here = (over: Partial<EditorParams>): EditorParams => ({ ...params, ...over });
  // The open table without an estimate shows the exact count its short page gave.
  const rowCount = (o: ObjectSummary, current: boolean) => o.row_estimate ?? (current ? exactRows : null);

  return (
    <nav aria-label="Tables" className="flex w-[248px] shrink-0 flex-col gap-3 border-r border-line bg-deep py-3">
      <div className="space-y-2 px-3">
        <Picker
          label="Schema"
          look="field"
          caption
          mono
          options={(schema && !schemas.includes(schema) ? [schema, ...schemas] : schemas).map((s) => ({ value: s, label: s }))}
          value={schema}
          onChange={(s) => navigate(here({ schema: s, table: undefined }))}
        />
        <input
          type="search"
          aria-label="Filter tables"
          placeholder="Filter tables"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="h-9 w-full border border-line-strong bg-transparent px-2.5 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
        />
      </div>
      <p className="px-4 text-xs tracking-wider text-chalk-dim uppercase">
        {objects ? `${schema ?? ""} · ${countLine(inSchema)}` : noDatabase ? "No database" : "Loading…"}
      </p>
      <ul className="min-h-0 flex-1 overflow-y-auto">
        {shown.map((o, i) => {
          const current = view === "table" && o.name === params.table;
          const kindId = o.kind !== "table" ? `${ids}-kind-${i}` : undefined;
          return (
            <li key={`${o.schema}.${o.name}`}>
              <button
                type="button"
                aria-label={o.name}
                aria-describedby={kindId}
                aria-current={current ? "page" : undefined}
                onClick={() => {
                  setView("table");
                  navigate(here({ table: o.name }));
                }}
                className={cn(
                  "relative flex h-9 w-full items-center justify-between gap-2 px-4 text-left font-mono text-[13px] outline-none hover:bg-chalk/[0.06] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand",
                  current ? "bg-channel text-chalk before:absolute before:inset-y-0 before:left-0 before:w-0.5 before:bg-sand" : "text-chalk-dim",
                )}
              >
                <span className="truncate">{o.name}</span>
                {o.kind !== "table" ? (
                  <span id={kindId} className="shrink-0 text-[11px]">{o.kind.replace("_", " ")}</span>
                ) : (
                  rowCount(o, current) !== null && <span className="shrink-0 text-xs">{formatCount(rowCount(o, current)!)}</span>
                )}
              </button>
            </li>
          );
        })}
        {objects && shown.length === 0 && (
          <li className="px-4 py-2 text-sm text-chalk-dim">{needle ? "No table matches." : "No tables in this schema."}</li>
        )}
      </ul>
      <div className="space-y-2 px-3">
        <button type="button" aria-pressed={view === "types"} disabled={noDatabase} title={noDatabase ? NO_DATABASE : undefined} onClick={() => setView("types")} className={cn(ghost, view === "types" && "bg-channel")}>
          Types
        </button>
        <button type="button" aria-pressed={view === "new_table"} disabled={noDatabase} title={noDatabase ? NO_DATABASE : undefined} onClick={() => setView("new_table")} className={cn(ghost, view === "new_table" && "bg-channel")}>
          New table
        </button>
      </div>
    </nav>
  );
}
