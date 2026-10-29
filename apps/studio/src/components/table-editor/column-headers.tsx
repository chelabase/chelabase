import { Check, Plus } from "lucide-react";
import type { ProjectedColumn, ProjectedTable } from "@/lib/table-editor/changeset";
import { useTableEditor, type FormField } from "@/lib/table-editor/store";
import { cn } from "@/lib/utils";
import { MenuItem } from "@/components/ui/menu";
import type { GridHeaders, HeaderView } from "./grid";
import type { TableTarget } from "./target";
import { useTableStructure } from "./use-structure";

const MARK_WORD = { added: "new", changed: "changed", dropped: "dropped" } as const;

/** What a chip's header reads after the column's name: `bigint`, `int8, new`. */
export function chipDescription(c: ProjectedColumn): string {
  return `${c.data_type}${c.mark ? `, ${MARK_WORD[c.mark]}` : ""}`;
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

/** The chips' configuration from the editor: why structure can't be changed here, or null. */
export interface ColumnChips {
  reason: string | null;
}

/**
 * The grid's header cells as column chips: a typed chip per column (a click opens its form; its `⋯` menu holds the
 * quick changes after the grid's sort items), pending marks (sand edge and "new" for added, sand edge for changed,
 * struck and coral for dropped, seafoam for just applied), then "Add column". Null without `chips`, or until the
 * table's structure is read: the grid keeps its plain headers.
 */
export function useColumnHeaders(target: TableTarget, chips: ColumnChips | undefined, opts: { enabled?: boolean } = {}): GridHeaders | null {
  const { base, projected, stage, ref, applying, reason: dropped } = useTableStructure(target, { enabled: !!chips && (opts.enabled ?? true) });
  const reason = chips?.reason ?? dropped;
  const openPanel = useTableEditor((s) => s.openPanel);
  const applied = useTableEditor((s) => (s.applied?.key === target.key ? s.applied.columns : null));
  if (!chips || !base || !projected) return null;

  const disabled = reason !== null;
  const byOriginal = new Map(projected.columns.filter((c) => c.original !== null).map((c) => [c.original!, c]));
  const added = new Map(projected.columns.filter((c) => c.original === null).map((c) => [c.name, c]));

  function edit(c: ProjectedColumn, field?: FormField) {
    if (c.mark === "dropped") {
      openPanel("changes");
      return;
    }
    openPanel("column", { column: c.name, focus: field });
  }

  const view = (c: ProjectedColumn): HeaderView => {
    const notes = chipNotes(c, projected);
    const isDropped = c.mark === "dropped";
    const fresh = applied?.includes(c.name) && c.mark === null;
    return {
      label: c.name,
      description: chipDescription(c),
      title: disabled ? reason : undefined,
      onActivate: disabled ? null : () => edit(c),
      className: cn(
        (c.mark === "added" || c.mark === "changed") && "shadow-[inset_2px_0_0_var(--sand)]",
        isDropped && "shadow-[inset_2px_0_0_var(--coral)]",
        fresh && "shadow-[inset_2px_0_0_var(--seafoam)]",
      ),
      content: (
        <span className="flex min-w-0 flex-col justify-center gap-0.5 font-mono text-[11px] leading-tight">
          <span className={cn("truncate text-chalk", isDropped && "text-chalk-dim line-through decoration-coral")}>{c.name}</span>
          {/* In a narrow column the notes give way before the type does. */}
          <span className="flex min-w-0 text-chalk-dim">
            <span title={c.data_type} className="max-w-full shrink-0 truncate">
              {shortType(c.data_type)}
            </span>
            {notes.length > 0 && <span className="min-w-0 truncate whitespace-pre">{` · ${notes.join(" · ")}`}</span>}
            {c.mark === "added" && <b className="ml-1 shrink-0 font-sans font-semibold text-sand">new</b>}
          </span>
        </span>
      ),
      // Structure changes: none for a dropped column, while structure is read-only, or while an Apply runs.
      menu:
        isDropped || disabled || applying ? undefined : (
          <>
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
          </>
        ),
    };
  };

  return {
    column: (name) => {
      const c = byOriginal.get(name);
      return c ? view(c) : undefined;
    },
    pending: (name) => {
      const c = added.get(name);
      return c ? view(c) : undefined;
    },
    end: (
      <button
        type="button"
        disabled={disabled || applying}
        title={disabled ? reason : applying ? "Applying changes…" : undefined}
        onClick={() => openPanel("column", { column: null })}
        className="flex h-7 items-center gap-1 border border-input bg-channel px-2 text-[13px] whitespace-nowrap text-chalk outline-none hover:border-sand focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand disabled:cursor-default disabled:opacity-50 disabled:hover:border-input"
      >
        <Plus aria-hidden="true" size={14} strokeWidth={1.5} />
        Add column
      </button>
    ),
  };
}
