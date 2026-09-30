import { useState, type FormEvent } from "react";
import type { BranchSummary, CreateBranchRequest } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import type { ApiClient } from "@/lib/api";
import { pointToUtc, restoreBounds, validateBranchName } from "@/lib/branches";

export interface NewBranchFormProps {
  api: ApiClient;
  databaseId: string;
  branches: BranchSummary[];
  historyWindow: number;
  /** Called once the API accepted the branch. */
  onCreated: () => void;
}

export function NewBranchForm({ api, databaseId, branches, historyWindow, onCreated }: NewBranchFormProps) {
  // Any ready branch can be a parent, restore backups included (they're marked).
  const parents = branches.filter((b) => b.status === "ready");
  const main = parents.find((b) => b.kind === "normal" && b.parent_branch_id === null);
  const [name, setName] = useState("");
  const [parent, setParent] = useState<string>(main?.id ?? parents[0]?.id ?? "");
  const [mode, setMode] = useState<"now" | "time">("now");
  const [time, setTime] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const nameError = name === "" ? null : validateBranchName(name);
  const since = parents.find((b) => b.id === parent)?.history_start;
  const bounds = restoreBounds(new Date(), historyWindow, since);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || name === "" || nameError || !parent) return;
    const request: CreateBranchRequest = { name, parent_branch_id: parent as never };
    if (mode === "time") {
      const at = pointToUtc(time, new Date(), historyWindow, since);
      if (!at.ok) {
        setError(at.error);
        return;
      }
      request.at = { timestamp: at.iso };
    }
    setBusy(true);
    setError(null);
    try {
      await api.createBranch(databaseId, request);
      setName("");
      setTime("");
      setMode("now");
      onCreated();
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
          <span>Branch name</span>
          <input value={name} onChange={(e) => setName(e.target.value)} className={`${field} w-56`} />
        </label>
        <label className="flex flex-col gap-1">
          <span>Parent</span>
          <select value={parent} onChange={(e) => setParent(e.target.value)} className={field}>
            {parents.map((b) => (
              <option key={b.id} value={b.id}>
                {b.kind === "backup" ? `${b.name} (backup)` : b.name}
              </option>
            ))}
          </select>
        </label>
        <fieldset className="flex items-center gap-3">
          <label className="flex items-center gap-1">
            <input type="radio" name="point" checked={mode === "now"} onChange={() => setMode("now")} />
            Now
          </label>
          <label className="flex items-center gap-1">
            <input type="radio" name="point" checked={mode === "time"} onChange={() => setMode("time")} />
            At a time
          </label>
        </fieldset>
        {mode === "time" && (
          <label className="flex flex-col gap-1">
            <span>Point in time</span>
            <input
              type="datetime-local"
              value={time}
              min={bounds.min}
              max={bounds.max}
              onChange={(e) => setTime(e.target.value)}
              className={field}
            />
          </label>
        )}
        <Button type="submit" size="sm" disabled={busy || name === "" || nameError !== null || !parent}>
          Create branch
        </Button>
      </div>
      {nameError && <p className="text-xs text-destructive">{nameError}</p>}
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
    </form>
  );
}
