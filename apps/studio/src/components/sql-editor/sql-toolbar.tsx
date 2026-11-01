import { Button } from "@/components/ui/button";
import { TIMEOUTS_MS } from "./run-history";

export interface SqlToolbarProps {
  timeoutMs: number;
  onTimeout: (ms: number) => void;
  running: boolean;
  canRun: boolean;
  onRun: () => void;
  onCancel: () => void;
  onRefreshSchema: () => void;
}

const field = "h-8 border border-input bg-background px-2 text-sm";
const TIMEOUT_LABEL: Record<number, string> = { 60_000: "60 s", 300_000: "5 min", 900_000: "15 min" };

export function SqlToolbar(p: SqlToolbarProps) {
  return (
    <div className="flex flex-wrap items-end gap-3 text-sm">
      <label className="flex flex-col gap-1">
        <span>Timeout</span>
        <select value={p.timeoutMs} onChange={(e) => p.onTimeout(Number(e.target.value))} className={field}>
          {TIMEOUTS_MS.map((ms) => (
            <option key={ms} value={ms}>
              {TIMEOUT_LABEL[ms]}
            </option>
          ))}
        </select>
      </label>
      {p.running ? (
        <Button variant="destructive" onClick={p.onCancel}>
          Cancel
        </Button>
      ) : (
        <Button disabled={!p.canRun} onClick={p.onRun} title="Ctrl/Cmd+Enter runs the selection, or everything">
          Run
        </Button>
      )}
      <Button variant="outline" onClick={p.onRefreshSchema}>
        Refresh schema
      </Button>
    </div>
  );
}
