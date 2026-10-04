import { useId, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { getGetDatabaseQueryKey, useUpdateDatabase, type DatabaseSettings as Settings } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { parseSettings, windowParts, type WindowUnit } from "@/lib/branches";

export interface DatabaseSettingsProps {
  databaseId: string;
  settings: Settings;
  /** Called after the save and the refetch of getDatabase(id). */
  onSaved?: (settings: Settings) => void;
}

export function DatabaseSettings({ databaseId, settings, onSaved }: DatabaseSettingsProps) {
  const queryClient = useQueryClient();
  const update = useUpdateDatabase({
    mutation: {
      // Awaited, so the mutation (and `busy`) waits for the refetch.
      onSuccess: async (done, { id }) => {
        await queryClient.invalidateQueries({ queryKey: getGetDatabaseQueryKey(id) });
        onSaved?.(done);
      },
    },
  });
  const initial = windowParts(settings.history_window);
  const [window, setWindow] = useState(String(initial.value));
  const [unit, setUnit] = useState<WindowUnit>(initial.unit);
  const [maxBranches, setMaxBranches] = useState(String(settings.max_branches));
  const [maxRows, setMaxRows] = useState(String(settings.max_rows));
  const [timeoutMs, setTimeoutMs] = useState(String(settings.statement_timeout_ms));
  const [maxConnections, setMaxConnections] = useState(String(settings.max_connections));
  const busy = update.isPending;
  const error = update.error ? errorText(update.error) : null;
  const saved = update.isSuccess;
  const reasonId = useId();
  const parsed = parseSettings({
    window,
    unit,
    maxBranches,
    maxRows,
    statementTimeoutMs: timeoutMs,
    maxConnections,
  });

  function submit(event: FormEvent) {
    event.preventDefault();
    if (!parsed.ok || busy) return;
    update.mutate({ id: databaseId, data: parsed.settings });
  }

  const field = "h-8 rounded-lg border border-input bg-background px-2 text-sm";
  return (
    <form onSubmit={submit} className="space-y-2 rounded-lg border p-3 text-sm">
      <div className="flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1">
          <span>History window</span>
          <input
            type="number"
            min={1}
            value={window}
            onChange={(e) => setWindow(e.target.value)}
            className={`${field} w-24`}
          />
        </label>
        <label className="flex flex-col gap-1">
          <span>Window unit</span>
          <select value={unit} onChange={(e) => setUnit(e.target.value as WindowUnit)} className={field}>
            <option value="hours">hours</option>
            <option value="days">days</option>
          </select>
        </label>
        <label className="flex flex-col gap-1">
          <span>Branch limit</span>
          <input
            type="number"
            min={0}
            max={20}
            value={maxBranches}
            onChange={(e) => setMaxBranches(e.target.value)}
            className={`${field} w-24`}
          />
        </label>
      </div>
      <div className="flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1">
          <span>Max rows</span>
          <input
            type="number"
            min={1}
            max={100000}
            value={maxRows}
            onChange={(e) => setMaxRows(e.target.value)}
            className={`${field} w-28`}
          />
        </label>
        <label className="flex flex-col gap-1">
          <span>Statement timeout (ms)</span>
          <input
            type="number"
            min={100}
            max={120000}
            value={timeoutMs}
            onChange={(e) => setTimeoutMs(e.target.value)}
            className={`${field} w-32`}
          />
        </label>
        <label className="flex flex-col gap-1">
          <span>Max connections</span>
          <input
            type="number"
            min={1}
            max={50}
            value={maxConnections}
            onChange={(e) => setMaxConnections(e.target.value)}
            className={`${field} w-24`}
          />
        </label>
        <Button
          type="submit"
          size="sm"
          disabled={!parsed.ok || busy}
          aria-describedby={!parsed.ok || busy ? reasonId : undefined}
        >
          Save settings
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">Shrinking the window lets older history be removed.</p>
      {(busy || !parsed.ok) && (
        <div id={reasonId} className="space-y-1">
          {busy ? (
            <p className="text-xs text-muted-foreground">Saving the settings…</p>
          ) : (
            !parsed.ok &&
            [
              parsed.window,
              parsed.maxBranches,
              parsed.maxRows,
              parsed.statementTimeoutMs,
              parsed.maxConnections,
            ]
              .filter((m): m is string => m !== undefined)
              .map((m) => (
                <p key={m} className="text-xs text-destructive">
                  {m}
                </p>
              ))
          )}
        </div>
      )}
      {saved && (
        <p role="status" className="text-xs text-muted-foreground">
          Saved.
        </p>
      )}
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
    </form>
  );
}
