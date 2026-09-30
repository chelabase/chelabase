import { useState, type FormEvent } from "react";
import type { DatabaseSettings as Settings } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import type { ApiClient } from "@/lib/api";
import { parseSettings, windowParts, type WindowUnit } from "@/lib/branches";

export interface DatabaseSettingsProps {
  api: ApiClient;
  databaseId: string;
  settings: Settings;
  onSaved: (settings: Settings) => void;
}

export function DatabaseSettings({ api, databaseId, settings, onSaved }: DatabaseSettingsProps) {
  const initial = windowParts(settings.history_window);
  const [window, setWindow] = useState(String(initial.value));
  const [unit, setUnit] = useState<WindowUnit>(initial.unit);
  const [maxBranches, setMaxBranches] = useState(String(settings.max_branches));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const parsed = parseSettings({ window, unit, maxBranches });

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!parsed.ok || busy) return;
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      const done = await api.updateDatabase(databaseId, parsed.settings);
      setSaved(true);
      onSaved(done);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
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
        <Button type="submit" size="sm" disabled={!parsed.ok || busy}>
          Save settings
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">Shrinking the window lets older history be removed.</p>
      {!parsed.ok && parsed.window && <p className="text-xs text-destructive">{parsed.window}</p>}
      {!parsed.ok && parsed.maxBranches && <p className="text-xs text-destructive">{parsed.maxBranches}</p>}
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
