import type { ColumnDesc } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { filterOpsFor, type FilterRow, type FilterSet } from "@/lib/console";

export interface ConsoleFiltersProps {
  columns: Record<string, ColumnDesc>;
  value: FilterSet;
  onChange: (next: FilterSet) => void;
  onApply: () => void;
  error: string | null;
}

const field = "h-8 rounded-lg border border-input bg-background px-2 text-sm";

export function ConsoleFilters({ columns, value, onChange, onApply, error }: ConsoleFiltersProps) {
  const names = Object.keys(columns);
  const patch = (i: number, p: Partial<FilterRow>) =>
    onChange({ ...value, rows: value.rows.map((r, j) => (j === i ? { ...r, ...p } : r)) });
  return (
    <div className="space-y-2 text-sm">
      <div className="flex flex-wrap items-center gap-2">
        <Button size="sm" variant="outline" onClick={() => onChange({ ...value, rows: [...value.rows, { col: names[0] ?? "", op: "eq", value: "" }] })}>
          Add filter
        </Button>
        {value.rows.length > 1 && (
          <label className="flex items-center gap-1">
            Match
            <select aria-label="Match" value={value.mode} onChange={(e) => onChange({ ...value, mode: e.target.value as "and" | "or" })} className={field}>
              <option value="and">all (and)</option>
              <option value="or">any (or)</option>
            </select>
          </label>
        )}
        <Button size="sm" variant="outline" onClick={onApply}>
          Apply filters
        </Button>
      </div>
      {value.rows.map((r, i) => (
        <div key={i} className="flex flex-wrap items-center gap-2">
          <select aria-label={`Filter column ${i + 1}`} value={r.col} onChange={(e) => {
              const next = e.target.value;
              patch(i, { col: next, ...(filterOpsFor(columns[next]).includes(r.op) ? {} : { op: "eq" as const, value: "" }) });
            }} className={field}>
            {names.map((n) => (
              <option key={n}>{n}</option>
            ))}
          </select>
          <select aria-label={`Filter operator ${i + 1}`} value={r.op} onChange={(e) => patch(i, { op: e.target.value as FilterRow["op"], value: "" })} className={field}>
            {filterOpsFor(columns[r.col]).map((o) => (
              <option key={o}>{o}</option>
            ))}
          </select>
          {r.op === "is" ? (
            <select aria-label={`Filter value ${i + 1}`} value={r.value} onChange={(e) => patch(i, { value: e.target.value })} className={field}>
              <option value="">Choose…</option>
              <option>null</option>
              <option>true</option>
              <option>false</option>
            </select>
          ) : (
            <input
              aria-label={`Filter value ${i + 1}`}
              value={r.value}
              placeholder={r.op === "in" ? "comma-separated" : undefined}
              onChange={(e) => patch(i, { value: e.target.value })}
              className={`${field} w-48`}
            />
          )}
          <Button size="sm" variant="ghost" aria-label={`Remove filter ${i + 1}`} onClick={() => onChange({ ...value, rows: value.rows.filter((_, j) => j !== i) })}>
            Remove
          </Button>
        </div>
      ))}
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}
