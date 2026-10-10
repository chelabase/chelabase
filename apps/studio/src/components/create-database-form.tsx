import { useEffect, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { getGetMeQueryKey, useCreateDatabase, useGetDatabase, type Database } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { pollWhile, TRANSITIONAL } from "@/lib/query";

export interface CreateDatabaseFormProps {
  projectId: string;
}

export function CreateDatabaseForm({ projectId }: CreateDatabaseFormProps) {
  const queryClient = useQueryClient();
  const [name, setName] = useState("");
  /** The database this form last created, followed until it settles. */
  const [created, setCreated] = useState<{ id: string; name: string } | null>(null);
  /** How it settled; once set, the form stops following the database (a later `deleting` isn't "Creating …"). */
  const [outcome, setOutcome] = useState<{ ready: true } | { ready: false; error: string } | null>(null);
  const trimmed = name.trim();

  const create = useCreateDatabase({
    mutation: {
      onSuccess: (accepted, { data }) => {
        setCreated({ id: accepted.database_id, name: data.name });
        // The dashboard lists it (and polls while it's provisioning). Returned so
        // the mutation (and `busy`) waits for the refetch.
        return queryClient.invalidateQueries({ queryKey: getGetMeQueryKey() });
      },
    },
  });
  const database = useGetDatabase(created?.id ?? "", {
    query: {
      enabled: created !== null && outcome === null,
      refetchInterval: pollWhile<Database>((db) => TRANSITIONAL.has(db.status)),
    },
  });

  const db = created && outcome === null ? database.data : undefined;
  const following = created !== null && outcome === null && (db === undefined ? !database.error : TRANSITIONAL.has(db.status));
  const busy = create.isPending || following;
  // Snapshot how it settled. As before, the name clears once it's ready (a failure keeps it for a retry).
  const settledStatus = db?.status === "ready" || db?.status === "failed" ? db.status : null;
  const settledError = db?.error ?? null;
  useEffect(() => {
    if (settledStatus === "ready") {
      setOutcome({ ready: true });
      setName("");
    } else if (settledStatus === "failed") {
      setOutcome({ ready: false, error: settledError ?? "Provisioning failed." });
    }
  }, [settledStatus, settledError]);
  const ready = outcome?.ready === true;
  const failure = outcome && !outcome.ready ? outcome.error : null;

  function submit(event: FormEvent) {
    event.preventDefault();
    if (!trimmed || busy) return;
    setCreated(null);
    setOutcome(null);
    create.mutate({ data: { project_id: projectId, name: trimmed } });
  }

  const working = create.isPending
    ? { name: create.variables.data.name, phase: "pending" }
    : following && created
      ? { name: created.name, phase: db?.status ?? "pending" }
      : null;
  const error = create.error
    ? errorText(create.error)
    : failure
      ? failure
      : created && outcome === null && database.error && !db
        ? errorText(database.error)
        : null;

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
      {working && (
        <p className="text-sm text-muted-foreground">
          Creating {working.name}… ({working.phase})
        </p>
      )}
      {created && ready && <p className="text-sm">{created.name} is ready</p>}
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
    </form>
  );
}
