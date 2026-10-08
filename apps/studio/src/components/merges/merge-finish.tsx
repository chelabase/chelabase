import { useId, useState } from "react";
import { Link } from "@tanstack/react-router";
import { fetchMergePlanSql, type AfterMerge, type Merge } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { describeAfter, itemName, leftoverFix, leftoverText, readResult } from "@/lib/merges";
import type { SaveMerge } from "./merge-page";

export interface MergeFinishProps {
  projectId: string;
  databaseId: string;
  merge: Merge;
  hasPlan: boolean;
  sourceName: string;
  targetName: string;
  editable: boolean;
  /** An action or save is in flight. */
  pending: boolean;
  save: SaveMerge;
  /** Runs the after-merge choice a drifted merge skipped (E21). */
  onFinish: (after: AfterMerge) => void;
}

const AFTER_VALUES: AfterMerge[] = ["keep", "reset", "delete"];

/** Saves `text` as a file through a temporary link (the plan never enters the query cache). */
function download(text: string, filename: string) {
  const url = URL.createObjectURL(new Blob([text], { type: "text/plain;charset=utf-8" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.append(a);
  a.click();
  a.remove();
  URL.revokeObjectURL(url);
}

/** The after-merge choice, the roles the merge created, differences after the apply, and the export. */
export function MergeFinish({
  projectId,
  databaseId,
  merge,
  hasPlan,
  sourceName,
  targetName,
  editable,
  pending,
  save,
  onFinish,
}: MergeFinishProps) {
  const savedAfter = (AFTER_VALUES as string[]).includes(merge.options.after as string) ? (merge.options.after as AfterMerge) : "keep";
  const [afterChoice, setAfter] = useState<AfterMerge>(savedAfter);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const groupId = useId();
  const result = readResult(merge.result);
  const intoParent = merge.direction === "into_parent";
  const [runAfter, setRunAfter] = useState<AfterMerge>(savedAfter);
  const afterLabel: Record<AfterMerge, string> = {
    keep: `Keep ${sourceName}`,
    reset: `Reset ${sourceName} to ${targetName}`,
    delete: `Delete ${sourceName}`,
  };

  async function guarded(action: () => Promise<void>) {
    setBusy(true);
    setError(null);
    try {
      await action();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }
  const exportPlan = () =>
    guarded(async () => {
      download(await fetchMergePlanSql(databaseId, merge.id), `merge-${merge.id}.sql`);
    });

  const after = result?.after ?? null;
  const afterText = describeAfter(after, { sourceName, targetName, direction: merge.direction });
  // A drifted merge skipped the choice; it runs by hand once the differences have been looked at.
  const skipped = merge.status === "applied_with_drift" && after?.skipped != null;
  const drift = result?.drift.filter((d) => !d.produced) ?? [];
  const produced = result?.drift.filter((d) => d.produced) ?? [];

  return (
    <div className="space-y-4 text-sm">
      {intoParent && after === null && (
        <fieldset aria-describedby={groupId} className="m-0 min-w-0 space-y-2 border-0 p-0">
          <legend className="font-medium">When the merge is done</legend>
          <p id={groupId} className="text-xs text-muted-foreground">
            What happens to {sourceName}. Reset and delete keep their usual refusals (a protected branch, child
            branches); a refusal is recorded and the merge still finishes. Saving the choice keeps the rehearsal.
          </p>
          <div className="flex flex-wrap gap-x-4 gap-y-1">
            {AFTER_VALUES.map((v) => (
              <label key={v} className="flex items-center gap-2">
                <input
                  type="radio"
                  name="after-merge"
                  value={v}
                  checked={afterChoice === v}
                  disabled={!editable}
                  onChange={() => setAfter(v)}
                />
                {afterLabel[v]}
              </label>
            ))}
          </div>
          <Button size="sm" variant="outline" disabled={!editable} onClick={() => void save({ options: { after: afterChoice } })}>
            Save after-merge choice
          </Button>
        </fieldset>
      )}

      {afterText && !skipped && <p>{afterText}</p>}

      {skipped && (
        <section aria-label="The after-merge choice was skipped" className="space-y-2 rounded-lg border p-3">
          <p>
            {targetName} doesn&apos;t read back as merged, so {sourceName} and its base were left as they were. Look at the
            differences below; if the merge still stands, run the choice now.
          </p>
          {intoParent ? (
            <div className="flex flex-wrap gap-x-4 gap-y-1">
              {AFTER_VALUES.map((v) => (
                <label key={v} className="flex items-center gap-2">
                  <input
                    type="radio"
                    name="run-after-merge"
                    value={v}
                    checked={runAfter === v}
                    disabled={pending}
                    onChange={() => setRunAfter(v)}
                  />
                  {afterLabel[v]}
                </label>
              ))}
            </div>
          ) : (
            <p className="text-xs text-muted-foreground">
              An update from the parent only keeps: the merge becomes {targetName}&apos;s new base, if {sourceName} is still
              the schema that was merged.
            </p>
          )}
          <Button size="sm" variant="outline" disabled={pending} onClick={() => onFinish(intoParent ? runAfter : "keep")}>
            Run the after-merge choice
          </Button>
        </section>
      )}

      {result?.leftovers != null && (
        <section aria-label={`Left on ${targetName}`} className="space-y-2">
          <h3 className="font-medium">Left on {targetName}</h3>
          {result.leftovers.length === 0 ? (
            <p>The online steps had left nothing behind.</p>
          ) : (
            <>
              <p className="text-xs text-muted-foreground">
                The merge was abandoned after its transaction committed. Nothing was undone. Clear each of these by hand
                with the SQL shown, or leave it.
              </p>
              <ul className="space-y-2">
                {result.leftovers.map((l) => {
                  const fix = leftoverFix(l);
                  return (
                    <li key={`${l.kind}:${l.schema}.${l.table}.${l.name}`} className="space-y-1 rounded-lg border p-3">
                      <p>
                        <span className="font-mono">
                          {l.schema}.{l.table}
                        </span>
                        : {leftoverText(l)}
                      </p>
                      {fix && (
                        <pre
                          tabIndex={0}
                          aria-label={`SQL to clear ${l.name}`}
                          className="overflow-auto rounded-lg border bg-background p-2 font-mono text-xs whitespace-pre-wrap"
                        >
                          {fix}
                        </pre>
                      )}
                    </li>
                  );
                })}
              </ul>
            </>
          )}
        </section>
      )}

      {result && result.created_roles.length > 0 && (
        <div className="space-y-2">
          <h3 className="font-medium">Roles created on {targetName}</h3>
          <p className="text-xs text-muted-foreground">
            Each has a new password, revealed from {targetName}&apos;s connection details like any other role&apos;s.
          </p>
          <ul className="space-y-1">
            {result.created_roles.map((r) => (
              <li key={r.name} className="flex flex-wrap items-center gap-2">
                <span className="font-mono">{r.name}</span>
                <Link
                  to="/app/projects/$projectId"
                  params={{ projectId }}
                  search={{ database: databaseId, branch: merge.target_branch_id, panel: "connection", role: r.name }}
                  aria-label={`Connection details for ${r.name} on ${targetName}`}
                  className="text-foreground underline underline-offset-4"
                >
                  Connection details
                </Link>
              </li>
            ))}
          </ul>
        </div>
      )}

      {drift.length > 0 && (
        <section aria-label="Differences after the apply" className="space-y-1">
          <h3 className="font-medium text-destructive">Differences after the apply</h3>
          <ul className="list-disc pl-5">
            {drift.map((d) => (
              <li key={`${d.key.kind}:${itemName(d.key)}`}>
                <span className="font-mono">{itemName(d.key)}</span>: {d.kind}
              </li>
            ))}
          </ul>
        </section>
      )}

      {produced.length > 0 && (
        <section aria-label="What your own SQL produced" className="space-y-1">
          <h3 className="font-medium">What your own SQL produced</h3>
          <p className="text-xs text-muted-foreground">
            Own SQL is checked against what it produced, not against the plan, so these aren&apos;t differences.
          </p>
          <ul className="list-disc pl-5">
            {produced.map((d) => (
              <li key={`${d.key.kind}:${itemName(d.key)}`} className="font-mono">
                {itemName(d.key)}
              </li>
            ))}
          </ul>
        </section>
      )}

      <div className="space-y-1">
        <Button size="sm" variant="outline" disabled={!hasPlan || busy} onClick={() => void exportPlan()}>
          Download the plan (.sql)
        </Button>
        <p className="text-xs text-muted-foreground">
          The plan as SQL, phase by phase: the transaction in BEGIN/COMMIT, the online steps marked to run outside one,
          roles as comments (never passwords).
        </p>
      </div>

      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}
