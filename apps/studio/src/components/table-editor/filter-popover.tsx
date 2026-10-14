import { useEffect, useId, useRef, useState } from "react";
import { X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { buildFilterTree, filterOpsFor, type FilterOp, type FilterRow, type Tree } from "@/lib/console";
import type { RowColumn } from "@/lib/table-editor/rows";
import { cn } from "@/lib/utils";
import { Picker } from "./picker";

export interface FilterPopoverProps {
  columns: RowColumn[];
  /** The conditions applied now (all must match). */
  value: FilterRow[];
  /** A new set of conditions and the where tree they build (undefined: no filter). */
  onChange: (rows: FilterRow[], where: Tree | undefined) => void;
}

const IS_VALUES = ["null", "true", "false"];

/**
 * The Filter button and its popover: the applied conditions (each removable), and one new
 * condition (column, operator, value) added with "Apply filter". Conditions combine with AND
 * into `lib/console`'s filter tree.
 */
export function FilterPopover({ columns, value, onChange }: FilterPopoverProps) {
  const [open, setOpen] = useState(false);
  const [col, setCol] = useState<string | null>(null);
  const [op, setOp] = useState<FilterOp>("eq");
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const panelId = useId();
  const buttonRef = useRef<HTMLButtonElement>(null);
  const byName = Object.fromEntries(columns.map((c) => [c.name, c.desc]));
  const ops = filterOpsFor(col ? byName[col] : undefined);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: globalThis.KeyboardEvent) => {
      // A picker's own Escape closes its list first.
      if (e.key === "Escape" && !e.defaultPrevented && !document.querySelector("[role=listbox]")) {
        setOpen(false);
        buttonRef.current?.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open]);

  function apply(rows: FilterRow[]): boolean {
    const built = buildFilterTree({ mode: "and", rows }, byName);
    if (!built.ok) {
      setError(built.error);
      return false;
    }
    setError(null);
    onChange(rows, built.where);
    return true;
  }

  function add() {
    if (!col) {
      setError("Pick a column to filter on.");
      return;
    }
    if (apply([...value, { col, op, value: text }])) {
      setText("");
      setOpen(false);
      buttonRef.current?.focus();
    }
  }

  return (
    <div className="relative">
      <Button
        ref={buttonRef}
        size="sm"
        variant="outline"
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        onClick={() => setOpen(!open)}
        className={cn(value.length > 0 && "border-chalk/60")}
      >
        Filter
        {value.length > 0 && (
          <span aria-hidden="true" className="border border-chalk/60 px-1 text-[11px] leading-4 text-chalk">
            {value.length}
          </span>
        )}
      </Button>
      {open && (
        <div
          id={panelId}
          role="group"
          aria-label="Filter rows"
          className="absolute top-full right-0 z-40 mt-1 w-[26rem] space-y-3 border border-line-strong bg-deep p-3 text-sm text-chalk"
        >
          {value.length > 0 && (
            <ul className="flex flex-wrap gap-1" aria-label="Applied filters">
              {value.map((f, i) => (
                <li key={`${f.col}-${i}`} className="flex items-center gap-1 border border-line-strong px-1.5 font-mono text-xs leading-6">
                  {f.col} {f.op} {f.value}
                  <button
                    type="button"
                    aria-label={`Remove filter ${f.col} ${f.op} ${f.value}`}
                    onClick={() => apply(value.filter((_, j) => j !== i))}
                    className="flex size-4 items-center justify-center text-chalk-dim hover:bg-chalk/[0.08] hover:text-chalk"
                  >
                    <X aria-hidden="true" size={12} strokeWidth={1.5} />
                  </button>
                </li>
              ))}
            </ul>
          )}
          <div className="grid grid-cols-[1fr_6rem] gap-2">
            <Picker
              label="Filter column"
              look="field"
              mono
              placeholder="Column"
              options={columns.map((c) => ({ value: c.name, label: c.name }))}
              value={col}
              onChange={(next) => {
                setCol(next);
                if (!filterOpsFor(byName[next]).includes(op)) setOp("eq");
              }}
            />
            <Picker
              label="Filter operator"
              look="field"
              mono
              options={ops.map((o) => ({ value: o, label: o }))}
              value={op}
              onChange={(next) => {
                setOp(next as FilterOp);
                setText("");
              }}
            />
          </div>
          {op === "is" ? (
            <Picker
              label="Filter value"
              look="field"
              mono
              placeholder="null, true or false"
              options={IS_VALUES.map((v) => ({ value: v, label: v }))}
              value={IS_VALUES.includes(text) ? text : null}
              onChange={setText}
            />
          ) : (
            <input
              aria-label="Filter value"
              value={text}
              placeholder={op === "in" ? "Comma-separated values" : "Value"}
              spellCheck={false}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  add();
                }
              }}
              className="h-9 w-full border border-input bg-ink px-2 font-mono text-[13px] text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
            />
          )}
          {error && (
            <p role="alert" className="text-coral">
              {error}
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button
              size="sm"
              variant="ghost"
              disabled={value.length === 0}
              onClick={() => {
                if (apply([])) setOpen(false);
              }}
            >
              Clear filters
            </Button>
            <Button size="sm" onClick={add}>
              Apply filter
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
