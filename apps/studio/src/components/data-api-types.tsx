import { useId, useState } from "react";
import type { BranchDatabaseView } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { downloadTypes, type TypesDatabase } from "@/lib/types-download";
import { CopyButton } from "./copy-button";

export interface DataApiTypesProps {
  databaseId: string;
  branchId: string;
  /** The branch's databases; only served ones can be typed. */
  databases: BranchDatabaseView[];
  /** The database selected until the user picks another here (the Connect panel's header choice). */
  defaultDatabase?: string;
}

const SNIPPET = `import { createClient, fetchTypes } from "@chelabase/client";
import type { Database } from "./db";

const db = createClient<Database>({ url, key: secretKey, types: await fetchTypes({ url, key: secretKey }) });`;

/** Download the branch's generated `db.ts` (what a secret key of the role would get), and how to use it. */
export function DataApiTypes({ databaseId, branchId, databases, defaultDatabase }: DataApiTypesProps) {
  const served = databases.filter((d) => d.origin !== "system" && d.data_api === "on");
  const [role, setRole] = useState("authenticated");
  // The select's value is an index into `choices`, so no database name can read as the "all" choice.
  const [picked, setPicked] = useState<number | null>(null);
  const [pickedFor, setPickedFor] = useState(defaultDatabase);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const choices: { label: string; database: TypesDatabase }[] = [
    ...served.map((d) => ({
      label: d.main ? `${d.name} (main)` : d.name,
      database: d.main ? ({ kind: "main" } as const) : ({ kind: "named", name: d.name } as const),
    })),
    { label: "All databases", database: { kind: "all" } },
  ];
  // Until one is picked: main, else the first served. A pick past the end (a database stopped
  // being served) falls back the same way.
  const wanted = served.findIndex((d) => d.name === defaultDatabase);
  const fallback = wanted >= 0 ? wanted : Math.max(0, choices.findIndex((c) => c.database.kind === "main"));
  // A change of the default (the header picker) takes over from an earlier local pick.
  const chosen = picked !== null && picked < choices.length && pickedFor === defaultDatabase ? picked : fallback;
  const database = choices[chosen].database;
  const reasonId = useId();
  const reason = role.trim() === "" ? "Enter a role." : served.length === 0 ? "No database is served yet." : null;

  async function download() {
    setBusy(true);
    setError(null);
    try {
      await downloadTypes(databaseId, branchId, { role, database });
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  const field = "h-8 border border-input bg-background px-2 text-sm";
  return (
    <section aria-label="Generated types" className="space-y-2">
      <h3 className="text-sm font-medium">Generated types</h3>
      <p className="text-xs text-muted-foreground">
        <code>db.ts</code> for this branch: the tables, views and functions a secret key with this role can reach.
      </p>
      <div className="flex flex-wrap items-end gap-3 text-sm">
        <label className="flex flex-col gap-1">
          <span>Role</span>
          <input value={role} onChange={(e) => setRole(e.target.value)} className={`${field} w-40`} />
        </label>
        {served.length > 1 && (
          <label className="flex flex-col gap-1">
            <span>Types database</span>
            <select value={String(chosen)} onChange={(e) => {
                setPicked(Number(e.target.value));
                setPickedFor(defaultDatabase);
              }} className={field}>
              {choices.map((c, i) => (
                <option key={i} value={String(i)}>
                  {c.label}
                </option>
              ))}
            </select>
          </label>
        )}
        <Button
          size="sm"
          disabled={reason !== null || busy}
          aria-describedby={reason ? reasonId : undefined}
          onClick={() => void download()}
        >
          {busy ? "Downloading…" : "Download types"}
        </Button>
      </div>
      {reason && (
        <p id={reasonId} className="text-xs text-muted-foreground">
          {reason}
        </p>
      )}
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
      <div className="space-y-1">
        <div className="flex items-center gap-2">
          <span className="text-xs text-muted-foreground">On a server, load the runtime types at startup:</span>
          <CopyButton value={SNIPPET} label="Copy snippet" />
        </div>
        <pre className="overflow-x-auto bg-muted p-2 text-xs">
          <code>{SNIPPET}</code>
        </pre>
      </div>
    </section>
  );
}
