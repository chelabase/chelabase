import { useState, type FormEvent } from "react";
import type { DatabaseStatus, ProjectId, TenantId } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import type { ApiClient } from "@/lib/api";

type FormState =
  | { status: "idle" }
  | { status: "working"; name: string; phase: DatabaseStatus }
  | { status: "done"; name: string }
  | { status: "error"; message: string };

export interface CreateDatabaseFormProps {
  api: ApiClient;
  tenantId: TenantId;
  projectId: ProjectId;
  /** Called whenever the database list should be refreshed. */
  onSettled: () => void;
  pollIntervalMs?: number;
  maxPolls?: number;
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

export function CreateDatabaseForm({
  api,
  tenantId,
  projectId,
  onSettled,
  pollIntervalMs = 2000,
  maxPolls = 90,
}: CreateDatabaseFormProps) {
  const [name, setName] = useState("");
  const [state, setState] = useState<FormState>({ status: "idle" });
  const trimmed = name.trim();
  const busy = state.status === "working";

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!trimmed || busy) return;
    setState({ status: "working", name: trimmed, phase: "pending" });
    try {
      const accepted = await api.createDatabase({
        tenant_id: tenantId,
        project_id: projectId,
        name: trimmed,
      });
      onSettled();
      for (let i = 0; i < maxPolls; i++) {
        await sleep(pollIntervalMs);
        const db = await api.getDatabase(accepted.database_id);
        if (db.status === "ready") {
          setState({ status: "done", name: trimmed });
          setName("");
          onSettled();
          return;
        }
        if (db.status === "failed") {
          setState({ status: "error", message: db.error ?? "Provisioning failed." });
          onSettled();
          return;
        }
        setState({ status: "working", name: trimmed, phase: db.status });
      }
      setState({
        status: "error",
        message: `${trimmed} is still provisioning; check the list again shortly.`,
      });
      onSettled();
    } catch (err) {
      setState({ status: "error", message: err instanceof Error ? err.message : String(err) });
    }
  }

  return (
    <form onSubmit={submit} className="flex flex-wrap items-end gap-3">
      <label className="flex flex-col gap-1 text-sm">
        <span>Database name</span>
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          disabled={busy}
          className="h-8 w-64 rounded-lg border border-input bg-background px-2"
          placeholder="web1"
        />
      </label>
      <Button type="submit" disabled={!trimmed || busy}>
        Create database
      </Button>
      {state.status === "working" && (
        <p className="text-sm text-muted-foreground">
          Creating {state.name}… ({state.phase})
        </p>
      )}
      {state.status === "done" && <p className="text-sm">{state.name} is ready</p>}
      {state.status === "error" && (
        <p role="alert" className="text-sm text-destructive">
          {state.message}
        </p>
      )}
    </form>
  );
}
