import { useId } from "react";
import type { Merge } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { itemName, openConflicts, readRehearsal, snapshotText, type Drift, type Plan } from "@/lib/merges";

export interface MergeRehearsalProps {
  merge: Merge;
  plan: Plan | null;
  targetName: string;
  /** An action or save is in flight. */
  pending: boolean;
  onRehearse: () => void;
}

const DRIFT_TEXT: Record<Drift["kind"], string> = { missing: "missing", unexpected: "not expected", different: "different" };

/** Why the merge can't be rehearsed now, or null. */
function rehearseReason(merge: Merge, plan: Plan | null): string | null {
  if (merge.status === "rehearsing") return "Rehearsing now.";
  if (merge.status === "rehearsed") return "Already rehearsed. Changing the choices, steps or lock timeout returns it to draft, to rehearse again; acknowledgements and the after-merge choice don't.";
  if (merge.status !== "draft") return "Only a draft merge can be rehearsed.";
  const open = openConflicts(merge).length;
  if (open > 0) return `Choose how to settle ${open} conflict${open === 1 ? "" : "s"}, and save.`;
  if (plan === null) return "The merge has no plan yet.";
  return null;
}

/** The rehearsal: run it, its timings, its failure, and what own SQL produced. */
export function MergeRehearsal({ merge, plan, targetName, pending, onRehearse }: MergeRehearsalProps) {
  const reasonId = useId();
  const rehearsal = readRehearsal(merge.rehearsal);
  const reason = rehearseReason(merge, plan);
  const failedSql = rehearsal?.failed_step ? plan?.steps.find((s) => s.id === rehearsal.failed_step)?.sql : undefined;
  const produced = rehearsal?.drift.filter((d) => d.produced) ?? [];
  const drift = rehearsal?.drift.filter((d) => !d.produced) ?? [];

  return (
    <div className="space-y-3 text-sm">
      <p className="text-muted-foreground">
        Runs the whole plan on a throwaway branch of {targetName}, with its real data, then compares the result with the
        merged schema. {targetName} doesn&apos;t change.
      </p>
      <div className="space-y-1">
        <Button
          size="sm"
          variant="outline"
          disabled={reason !== null || pending}
          aria-describedby={reason ? reasonId : undefined}
          onClick={onRehearse}
        >
          Rehearse
        </Button>
        {reason && (
          <p id={reasonId} className="text-xs text-muted-foreground">
            {reason}
          </p>
        )}
      </div>

      {rehearsal?.failed_step ? (
        <div role="alert" className="space-y-1 rounded-lg border border-destructive/60 p-3">
          <p className="font-medium text-destructive">
            The rehearsal failed at step {rehearsal.failed_step}. Nothing changed on {targetName}.
          </p>
          {failedSql && (
            <pre
              tabIndex={0}
              aria-label={`SQL of the failed step ${rehearsal.failed_step}`}
              className="overflow-auto rounded-lg border bg-background p-2 font-mono text-xs whitespace-pre-wrap"
            >
              {failedSql}
            </pre>
          )}
          {rehearsal.error && (
            <pre tabIndex={0} aria-label="Postgres's error" className="overflow-auto font-mono text-xs whitespace-pre-wrap">
              {rehearsal.error}
            </pre>
          )}
          <p className="text-xs text-muted-foreground">Fix it with a choice or a step of your own, then rehearse again.</p>
        </div>
      ) : (
        merge.status === "rehearsed" &&
        (drift.length > 0 ? (
          <p>
            The rehearsal ran, but its result differs from the merged schema (below). Look the differences over before you
            apply.
          </p>
        ) : (
          <p>The rehearsal passed.</p>
        ))
      )}

      {rehearsal && rehearsal.steps.length > 0 && (
        <table aria-label="Step timings" className="w-full text-left">
          <thead className="border-b text-muted-foreground">
            <tr>
              <th className="py-1 font-medium">Step</th>
              <th className="py-1 font-medium">Outcome</th>
              <th className="py-1 font-medium">Time</th>
            </tr>
          </thead>
          <tbody>
            {rehearsal.steps.map((s) => (
              <tr key={s.id} className="border-b last:border-0">
                <td className="py-1 font-mono">{s.id}</td>
                <td className={s.outcome === "failed" ? "py-1 text-destructive" : "py-1"}>{s.outcome ?? "—"}</td>
                <td className="py-1">{s.duration_ms === null ? "—" : `${s.duration_ms} ms`}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {produced.length > 0 && (
        <section aria-label="What your own SQL produced" className="space-y-1">
          <h3 className="font-medium">What your own SQL produced</h3>
          <p className="text-xs text-muted-foreground">
            Own SQL is checked against what it produced, not against the plan: look these over before you apply.
          </p>
          <ul className="space-y-2">
            {produced.map((d) => (
              <li key={`${d.key.kind}:${itemName(d.key)}`} className="space-y-1">
                <span className="font-mono">{itemName(d.key)}</span>
                <pre
                  tabIndex={0}
                  aria-label={`What your own SQL produced for ${itemName(d.key)}`}
                  className="max-h-48 overflow-auto rounded-lg border bg-background p-2 font-mono text-xs"
                >
                  {snapshotText(d.actual)}
                </pre>
              </li>
            ))}
          </ul>
        </section>
      )}

      {drift.length > 0 && (
        <section aria-label="Differences from the expected schema" className="space-y-1">
          <h3 className="font-medium text-destructive">Differences from the expected schema</h3>
          <ul className="list-disc pl-5">
            {drift.map((d) => (
              <li key={`${d.key.kind}:${itemName(d.key)}`}>
                <span className="font-mono">{itemName(d.key)}</span>: {DRIFT_TEXT[d.kind] ?? d.kind}
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
