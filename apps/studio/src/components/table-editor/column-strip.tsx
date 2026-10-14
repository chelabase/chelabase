import { useMemo } from "react";
import { Check, MoreHorizontal, Plus } from "lucide-react";
import type { ProjectedColumn, ProjectedTable } from "@/lib/table-editor/changeset";
import { useTableEditor, type FormField } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { Menu, MenuItem } from "@/components/ui/menu";
import type { GridLayout } from "./grid";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

const MARK_WORD = { added: "new", changed: "changed", dropped: "dropped" } as const;

/** The accessible name of a column's chip: `amount_cents bigint`, `discount_cents int8, new`. */
export function chipName(c: ProjectedColumn): string {
  return `${c.name} ${c.data_type}${c.mark ? `, ${MARK_WORD[c.mark]}` : ""}`;
}

const SHORT_TYPES: Record<string, string> = {
  bigint: "int8",
  integer: "int4",
  smallint: "int2",
  boolean: "bool",
  "double precision": "float8",
  real: "float4",
};
const SHORT_PATTERNS: readonly [RegExp, string][] = [
  [/^timestamp(\(\d+\))? with time zone$/, "timestamptz"],
  [/^timestamp(\(\d+\))? without time zone$/, "timestamp"],
  [/^time(\(\d+\))? with time zone$/, "timetz"],
  [/^time(\(\d+\))? without time zone$/, "time"],
  [/^character varying(\(\d+\))?$/, "varchar"],
  [/^character(\(\d+\))?$/, "char"],
  [/^bit varying(\(\d+\))?$/, "varbit"],
];

/** Postgres's short name for a type as `format_type` spells it: `bigint` → `int8`, `character varying(80)[]` → `varchar(80)[]`. */
export function shortType(dataType: string): string {
  const array = /(\[\])+$/.exec(dataType)?.[0] ?? "";
  const base = array ? dataType.slice(0, -array.length) : dataType;
  const exact = SHORT_TYPES[base];
  if (exact) return exact + array;
  for (const [re, short] of SHORT_PATTERNS) {
    const m = re.exec(base);
    if (m) return short + (m[1] ?? "") + array;
  }
  return dataType;
}

/** The short marks under a chip's name: key, foreign key, nullable. */
function chipNotes(c: ProjectedColumn, table: ProjectedTable): string[] {
  const live = table.constraints.filter((k) => k.mark !== "dropped" && k.columns.includes(c.name));
  const notes: string[] = [];
  if (live.some((k) => k.kind === "primary_key")) notes.push("key");
  if (live.some((k) => k.kind === "foreign_key")) notes.push("→ fk");
  if (!c.not_null) notes.push("null");
  return notes;
}

export interface ColumnStripProps {
  target: TableTarget;
  /** The grid's geometry: chips sit over their grid columns and share its scroll. */
  layout: GridLayout;
  /** Why structure can't be changed here, or null. */
  reason: string | null;
}

/**
 * The column strip over the grid: a typed chip per column (opening its form, with a menu of quick
 * changes), pending marks (sand edge and "new" for added, sand edge for changed, struck and coral for
 * dropped, seafoam for just applied), then "Add column".
 */
export function ColumnStrip({ target, layout, reason: structureReason }: ColumnStripProps) {
  const { base, projected, stage, ref, applying, reason: dropped } = useTableStructure(target);
  const reason = structureReason ?? dropped;
  const openPanel = useTableEditor((s) => s.openPanel);
  const applied = useTableEditor((s) => (s.applied?.key === target.key ? s.applied.columns : null));
  // Grid columns are the base table's columns in attnum order.
  const gridIndex = useMemo(() => {
    const m = new Map<string, number>();
    [...(base?.columns ?? [])].sort((a, b) => a.attnum - b.attnum).forEach((c, i) => m.set(c.name, i));
    return m;
  }, [base]);
  if (!base || !projected) return null;

  const disabled = reason !== null;
  const visible = new Set(layout.visibleCols);
  const added = projected.columns.filter((c) => c.original === null);
  const firstExtra = gridIndex.size + 2;
  // The grid's template already has a track per added column (its pending header and placeholder cells).
  const template = `${layout.template} max-content`;

  function edit(c: ProjectedColumn, field?: FormField) {
    if (c.mark === "dropped") {
      openPanel("changes");
      return;
    }
    openPanel("column", { column: c.name, focus: field });
  }

  const chip = (c: ProjectedColumn, column: number) => {
    const notes = chipNotes(c, projected);
    const dropped = c.mark === "dropped";
    const fresh = applied?.includes(c.name) && c.mark === null;
    return (
      <div
        key={`${c.original ?? "+"}:${c.name}`}
        role="listitem"
        style={{ gridColumn: column }}
        className={cn(
          "group/chip relative flex h-9 min-w-0 items-stretch border-r border-line",
          (c.mark === "added" || c.mark === "changed") && "shadow-[inset_2px_0_0_var(--sand)]",
          dropped && "shadow-[inset_2px_0_0_var(--coral)]",
          fresh && "shadow-[inset_2px_0_0_var(--seafoam)]",
        )}
      >
        <button
          type="button"
          aria-label={chipName(c)}
          title={disabled ? reason : undefined}
          disabled={disabled}
          onClick={() => edit(c)}
          className="flex min-w-0 flex-1 flex-col justify-center gap-0.5 px-2 text-left font-mono text-[11px] leading-tight outline-none hover:bg-chalk/[0.05] focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand disabled:cursor-default disabled:hover:bg-transparent"
        >
          <span className={cn("truncate text-chalk", dropped && "text-chalk-dim line-through decoration-coral")}>{c.name}</span>
          <span className="flex min-w-0 text-chalk-dim">
            <span title={c.data_type} className="truncate">
              {shortType(c.data_type)}
            </span>
            {notes.length > 0 && <span className="shrink-0 whitespace-pre">{` · ${notes.join(" · ")}`}</span>}
            {c.mark === "added" && <b className="ml-1 shrink-0 font-sans font-semibold text-sand">new</b>}
          </span>
        </button>
        {!dropped && (disabled || applying) && (
          <button type="button" disabled aria-label={`Column actions for ${c.name}`} title={reason ?? "Applying changes…"} className="flex w-6 shrink-0 items-center justify-center text-chalk-dim opacity-40">
            <MoreHorizontal aria-hidden="true" size={14} strokeWidth={1.5} />
          </button>
        )}
        {!dropped && !disabled && !applying && (
          <Menu
            align="start"
            trigger={
              <button
                type="button"
                aria-label={`Column actions for ${c.name}`}
                className="flex w-6 shrink-0 items-center justify-center text-chalk-dim opacity-60 outline-none group-hover/chip:opacity-100 hover:text-chalk focus-visible:opacity-100 focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand aria-expanded:opacity-100"
              >
                <MoreHorizontal aria-hidden="true" size={14} strokeWidth={1.5} />
              </button>
            }
          >
            <MenuItem onClick={() => edit(c, "name")}>Rename</MenuItem>
            <MenuItem onClick={() => edit(c, "type")}>Change type</MenuItem>
            <MenuItem onClick={() => edit(c, "default")}>Default</MenuItem>
            <MenuItem onClick={() => stage([{ alter_column: { table: ref, column: c.name, change: { not_null: !c.not_null } } }])}>
              <span className="flex-1">Not null</span>
              {c.not_null && <Check aria-hidden="true" size={14} strokeWidth={1.5} className="text-sand" />}
            </MenuItem>
            <MenuItem onClick={() => edit(c, "comment")}>Comment</MenuItem>
            <MenuItem
              destructive
              onClick={() => {
                stage([{ drop_column: { table: ref, column: c.name } }]);
                openPanel("changes");
              }}
            >
              Drop
            </MenuItem>
          </Menu>
        )}
      </div>
    );
  };

  return (
    <div
      role="list"
      aria-label="Columns"
      className="grid h-11 items-center border-b border-line-strong bg-deep"
      style={{ gridTemplateColumns: template, width: layout.totalWidth, minWidth: "100%" }}
    >
      {/* Over the gutter: keeps the chips aligned when the grid scrolls sideways. */}
      <div aria-hidden="true" className="sticky left-0 z-10 h-full border-r border-line bg-deep" style={{ gridColumn: 1 }} />
      {projected.columns.map((c) => {
        if (c.original === null) return chip(c, firstExtra + added.indexOf(c));
        const i = gridIndex.get(c.original);
        return i !== undefined && visible.has(i) ? chip(c, i + 2) : null;
      })}
      <div role="listitem" style={{ gridColumn: firstExtra + added.length }} className="px-2">
        <button
          type="button"
          disabled={disabled || applying}
          title={disabled ? reason : applying ? "Applying changes…" : undefined}
          onClick={() => openPanel("column", { column: null })}
          className="flex h-8 items-center gap-1 border border-input bg-channel px-2 text-[13px] whitespace-nowrap text-chalk outline-none hover:border-sand focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand disabled:cursor-default disabled:opacity-50 disabled:hover:border-input"
        >
          <Plus aria-hidden="true" size={14} strokeWidth={1.5} />
          Add column
        </button>
      </div>
    </div>
  );
}
