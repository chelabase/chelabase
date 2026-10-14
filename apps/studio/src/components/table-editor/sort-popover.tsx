import { useEffect, useId, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import type { Sort } from "@/lib/console";
import type { RowColumn } from "@/lib/table-editor/rows";
import { cn } from "@/lib/utils";
import { Picker } from "./picker";

export interface SortPopoverProps {
  columns: RowColumn[];
  /** The sort applied now (also set by a click on a column header), or null. */
  value: Sort | null;
  onChange: (sort: Sort | null) => void;
}

const DIRECTIONS = [
  { value: "asc", label: "Ascending" },
  { value: "desc", label: "Descending" },
] as const;

/**
 * The Sort button and its popover: a column and a direction, applied with "Apply sort" or removed
 * with "Clear sort". It edits the same sort a column header's click cycles.
 */
export function SortPopover({ columns, value, onChange }: SortPopoverProps) {
  const [open, setOpen] = useState(false);
  const [col, setCol] = useState<string | null>(value?.col ?? null);
  const [dir, setDir] = useState<Sort["dir"]>(value?.dir ?? "asc");
  const [error, setError] = useState<string | null>(null);
  const panelId = useId();
  const buttonRef = useRef<HTMLButtonElement>(null);

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

  function toggle() {
    if (!open) {
      // Opens on the sort in effect (a header click may have changed it).
      setCol(value?.col ?? null);
      setDir(value?.dir ?? "asc");
      setError(null);
    }
    setOpen(!open);
  }

  function close(next: Sort | null) {
    onChange(next);
    setOpen(false);
    buttonRef.current?.focus();
  }

  return (
    <div className="relative">
      <Button
        ref={buttonRef}
        size="sm"
        variant="outline"
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        onClick={toggle}
        className={cn(value && "border-chalk/60")}
      >
        Sort
        {value && (
          <span aria-hidden="true" className="font-mono text-xs text-chalk-dim">
            {value.col} {value.dir}
          </span>
        )}
      </Button>
      {open && (
        <div
          id={panelId}
          role="group"
          aria-label="Sort rows"
          className="absolute top-full right-0 z-40 mt-1 w-[22rem] space-y-3 border border-line-strong bg-deep p-3 text-sm text-chalk"
        >
          <div className="grid grid-cols-[1fr_8rem] gap-2">
            <Picker
              label="Sort column"
              look="field"
              mono
              placeholder="Column"
              options={columns.map((c) => ({ value: c.name, label: c.name }))}
              value={col}
              onChange={(next) => {
                setCol(next);
                setError(null);
              }}
            />
            <Picker label="Sort direction" look="field" options={DIRECTIONS} value={dir} onChange={(next) => setDir(next as Sort["dir"])} />
          </div>
          {error && (
            <p role="alert" className="text-coral">
              {error}
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button size="sm" variant="ghost" disabled={value === null} onClick={() => close(null)}>
              Clear sort
            </Button>
            <Button
              size="sm"
              onClick={() => {
                if (!col) {
                  setError("Pick a column to sort by.");
                  return;
                }
                close({ col, dir });
              }}
            >
              Apply sort
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
