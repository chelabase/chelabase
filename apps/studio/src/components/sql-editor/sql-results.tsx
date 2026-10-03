import { useState } from "react";
import { Button } from "@/components/ui/button";
import type { RunState, StatementResult } from "@/lib/sql-run";

export interface SqlResultsProps {
  state: RunState;
  /** Set when the run's error has a position in the editor. */
  onGoToError?: (() => void) | null;
}

const MESSAGES = "messages";

function Cell({ value }: { value: string | null }) {
  return value === null ? <span className="rounded bg-muted px-1 text-xs text-muted-foreground">NULL</span> : <>{value}</>;
}

function Grid({ result }: { result: StatementResult }) {
  return (
    <div className="max-h-96 overflow-auto rounded-lg border">
      <table className="w-full text-left font-mono text-xs">
        <thead className="sticky top-0 bg-muted">
          <tr>
            {result.columns.map((c, i) => (
              <th key={i} className="px-2 py-1 font-medium whitespace-nowrap" title={c.type}>
                {c.name}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {result.rows.map((row, r) => (
            <tr key={r} className="border-t">
              {row.map((v, i) => (
                <td key={i} className="max-w-[28rem] truncate px-2 py-1 whitespace-nowrap">
                  <Cell value={v} />
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function SqlResults({ state, onGoToError }: SqlResultsProps) {
  const [picked, setPicked] = useState<string | null>(null);
  const withColumns = state.statements.filter((s) => s.columns.length > 0);
  const tabs = [
    ...withColumns.map((s) => ({ id: `s${s.index}`, label: s.tag ?? "Running…", result: s })),
    { id: MESSAGES, label: "Messages", result: null },
  ];
  const active = tabs.find((t) => t.id === picked) ?? (state.error && withColumns.length === 0 ? tabs[tabs.length - 1] : tabs[0]);
  const { done, error } = state;

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-3 text-sm">
        <p role="status">
          {state.running ? "Running…" : done ? `${done.outcome === "ok" ? "ok" : done.outcome.replace("_", " ")} in ${done.duration_ms} ms` : ""}
        </p>
        {error && onGoToError && (
          <Button size="xs" variant="outline" onClick={onGoToError}>
            Go to error
          </Button>
        )}
      </div>
      <div role="tablist" className="flex flex-wrap gap-1 border-b">
        {tabs.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            aria-selected={active.id === t.id}
            onClick={() => setPicked(t.id)}
            className={`-mb-px border-b-2 px-2 py-1 text-sm ${active.id === t.id ? "border-primary font-medium" : "border-transparent text-muted-foreground"}`}
          >
            {t.label}
          </button>
        ))}
      </div>
      {active.result ? (
        <div className="space-y-2">
          {active.result.truncated && <p className="rounded-lg border p-2 text-sm">Showing the first 1,000 rows</p>}
          <Grid result={active.result} />
        </div>
      ) : (
        <ul className="space-y-1 font-mono text-xs">
          {state.notices.map((n, i) => (
            <li key={`n${i}`}>
              {n.severity}: {n.message}
              {n.detail ? ` (${n.detail})` : ""}
              {n.hint ? ` Hint: ${n.hint}` : ""}
            </li>
          ))}
          {state.statements
            .filter((s) => s.tag !== undefined)
            .map((s) => (
              <li key={`c${s.index}`}>
                {s.tag}
                {s.columns.length === 0 && s.rowCount ? ` (${s.rowCount} rows)` : ""}
              </li>
            ))}
          {error && (
            <li role="alert" className="text-destructive">
              ERROR {error.code}: {error.message}
              {error.detail ? ` (${error.detail})` : ""}
              {error.hint ? ` Hint: ${error.hint}` : ""}
            </li>
          )}
          {state.notices.length === 0 && state.statements.every((s) => s.tag === undefined) && !error && <li className="text-muted-foreground">No messages.</li>}
        </ul>
      )}
    </div>
  );
}
