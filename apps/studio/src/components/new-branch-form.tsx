import { useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useCreateBranch, type Branch, type CreateBranchRequest } from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import { invalidateBranches, pointToUtc, restoreBounds, validateBranchName } from "@/lib/branches";
import { errorText } from "@/lib/errors";

export interface NewBranchFormProps {
  databaseId: string;
  branches: Branch[];
  historyWindow: number;
}

export function NewBranchForm({ databaseId, branches, historyWindow }: NewBranchFormProps) {
  const queryClient = useQueryClient();
  const createBranch = useCreateBranch({
    mutation: {
      // Returned, so the mutation (and `busy`) waits for the refetch.
      onSuccess: (_, { id }) => invalidateBranches(queryClient, id),
    },
  });
  // Any ready branch can be a parent, restore backups included (they're marked).
  const parents = branches.filter((b) => b.status === "ready");
  const main = parents.find((b) => b.kind === "normal" && b.parent_branch_id === null);
  const [name, setName] = useState("");
  const [parent, setParent] = useState<string>(main?.id ?? parents[0]?.id ?? "");
  const [mode, setMode] = useState<"now" | "time">("now");
  const [time, setTime] = useState("");
  const [error, setError] = useState<string | null>(null);
  const busy = createBranch.isPending;
  const nameError = name === "" ? null : validateBranchName(name);
  const since = parents.find((b) => b.id === parent)?.history_start;
  const bounds = restoreBounds(new Date(), historyWindow, since);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || name === "" || nameError || !parent) return;
    const request: CreateBranchRequest = { name, parent_branch_id: parent };
    if (mode === "time") {
      const at = pointToUtc(time, new Date(), historyWindow, since);
      if (!at.ok) {
        setError(at.error);
        return;
      }
      request.at = { timestamp: at.iso };
    }
    setError(null);
    try {
      await createBranch.mutateAsync({ id: databaseId, data: request });
      setName("");
      setTime("");
      setMode("now");
    } catch (err) {
      setError(errorText(err));
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
