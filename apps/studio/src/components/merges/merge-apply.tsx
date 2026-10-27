import { useId, useState } from "react";
import type { Merge, MergeLogEntry } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { PHASES, PHASE_LABEL, appliedSteps, applyBlockers, type Phase, type Plan } from "@/lib/merges";
import { ErrorBody } from "./merge-error";

export interface MergeApplyProps {
  merge: Merge;
  plan: Plan | null;
  events: MergeLogEntry[];
  targetName: string;
  targetProtected: boolean;
  /** Whether `targetProtected` is known yet: Apply waits for the branch list. */
  branchesLoaded: boolean;
  /** An action or save is in flight. */
  pending: boolean;
  /** Applies; resolves to "confirm" when the API asks for the protected-target confirm. */
  onApply: (confirm: boolean) => Promise<"confirm" | undefined>;
  /** Retries the apply (E20): from the start after a roles, enum or transaction failure, else the online steps. */
  onRetry: () => void;
  /** Gives up a merge that failed in its online phase (E27). */
  onAbandon: () => void;
}

/** Once the apply started, progress by phase is shown. */
const STARTED = new Set<Merge["status"]>(["applying", "online", "finishing", "done", "applied_with_drift"]);
/** No Apply button: the apply ran or is running, or the merge ended another way. */
const ENDED_OR_RUNNING = new Set<Merge["status"]>([...STARTED, "failed", "cancelled"]);

/** The phases a Retry runs the whole apply again after: nothing of the plan's transaction is on the target. */
const RESTARTABLE = new Set<string>(["roles", "enum_add", "tx"]);
const WHAT_RAN: Record<string, (target: string) => string> = {
  roles: (t) => `Some roles may already be on ${t}; the plan's transaction didn't run.`,
  enum_add: (t) => `Some enum values are already on ${t}; the plan's transaction didn't run.`,
  tx: () => "The plan's transaction should have rolled back; Retry checks first whether it committed after all.",
};

const clock = (iso: string) => new Date(iso).toISOString().slice(11, 19);

/** One log line in words. */
function logLine(e: MergeLogEntry): string {
  const detail = e.detail ?? {};
  if (e.kind === "step") {
    const time = e.duration_ms === null ? "" : ` (${e.duration_ms} ms)`;
    return `step ${e.step_id ?? "?"}: ${e.outcome ?? "?"}${time}`;
  }
  const text = [detail.status, detail.to, detail.message, detail.text].find((v): v is string => typeof v === "string");
  if (e.kind === "status") return `status: ${text ?? ""}`.trim();
  return text ?? (Object.keys(detail).length > 0 ? JSON.stringify(detail) : e.kind);
}

/** Apply: the button and its refusals, the protected-target confirm, progress by phase and the log. */
export function MergeApply({
  merge,
  plan,
  events,
  targetName,
  targetProtected,
  branchesLoaded,
  pending,
  onApply,
  onRetry,
  onAbandon,
}: MergeApplyProps) {
  const [confirming, setConfirming] = useState(false);
  const [abandoning, setAbandoning] = useState(false);
  const reasonId = useId();
  const refusals = applyBlockers(merge);
  const blockers = refusals.length === 0 && !branchesLoaded ? ["Loading the branches…"] : refusals;
  const steps = plan?.steps ?? [];
  const done = appliedSteps(events, steps);
  const showProgress = steps.length > 0 && (STARTED.has(merge.status) || (merge.status === "failed" && merge.failed_phase !== null));

  function start() {
    if (targetProtected) {
      setConfirming(true);
      return;
    }
    void onApply(false).then((asked) => {
      if (asked === "confirm") setConfirming(true);
    });
  }

  return (
    <div className="space-y-3 text-sm">
      <p className="text-muted-foreground">
        Roles first, then the plan&apos;s changes in one transaction on {targetName} (all or nothing), then the online steps
        (new indexes, constraint validations) one at a time without blocking writes.
      </p>
      {!ENDED_OR_RUNNING.has(merge.status) && !confirming && (
        <div className="space-y-1">
          <Button
            size="sm"
            disabled={blockers.length > 0 || pending}
            aria-describedby={blockers.length > 0 ? reasonId : undefined}
            onClick={start}
          >
            Apply to {targetName}
          </Button>
          {blockers.length > 0 && (
            <p id={reasonId} className="text-xs text-muted-foreground">
              {blockers.join(" ")}
            </p>
          )}
        </div>
      )}

      {confirming && (
        <section aria-label="Confirm the apply" className="space-y-2 rounded-lg border border-destructive/60 p-3">
          <p>
            {targetName} is protected. Applying changes its schema for everything that uses it: the rehearsed plan runs
            on {targetName} itself, and there is no undo from here.
          </p>
          <div className="flex gap-2">
            <Button
              size="sm"
              disabled={pending}
              onClick={() => {
                setConfirming(false);
                void onApply(true);
              }}
            >
              Apply to protected {targetName}
            </Button>
            <Button size="sm" variant="outline" onClick={() => setConfirming(false)}>
              Cancel
            </Button>
          </div>
        </section>
      )}

      {merge.status === "failed" && merge.failed_phase !== null && RESTARTABLE.has(merge.failed_phase) && (
        <div role="alert" className="space-y-2 rounded-lg border border-destructive/60 p-3">
          <p className="text-destructive">
            The apply failed in {PHASE_LABEL[merge.failed_phase as Phase].toLowerCase()}. {WHAT_RAN[merge.failed_phase](targetName)}
          </p>
          {merge.error && <ErrorBody error={merge.error} />}
          <p>
            A retry runs the apply again from the start, after checking that the transaction didn&apos;t commit. Roles and
            enum values the first attempt made stay on {targetName}; the retry finds them and goes on. To plan the merge
            again instead, recompute it.
          </p>
          <Button size="sm" variant="outline" disabled={pending} onClick={onRetry}>
            Retry the apply
          </Button>
        </div>
      )}

      {merge.status === "failed" && merge.failed_phase === "online" && (
        <div role="alert" className="space-y-2 rounded-lg border border-destructive/60 p-3">
          <p className="text-destructive">
            The plan&apos;s transaction is committed: {targetName} already changed. Only the online steps (concurrent
            indexes and constraint validations) are left. A retry continues where they stopped. Until you retry to the end
            or abandon the merge, it holds both branches: they can&apos;t be reset, restored or deleted, and no other merge
            can start on them.
          </p>
          {merge.error && <ErrorBody error={merge.error} />}
          <div className="flex flex-wrap gap-2">
            <Button size="sm" variant="outline" disabled={pending} onClick={onRetry}>
              Retry the online steps
            </Button>
            {!abandoning && (
              <Button size="sm" variant="outline" disabled={pending} onClick={() => setAbandoning(true)}>
                Abandon the merge
              </Button>
            )}
          </div>
          {abandoning && (
            <section aria-label="Confirm the abandon" className="space-y-2 rounded-lg border border-destructive/60 p-3">
              <p>
                Abandoning gives up the online steps. {targetName} keeps everything the plan&apos;s transaction changed, and
                whatever the online steps left half done. Nothing is undone. The merge becomes cancelled and both branches are
                free again. What it left behind is listed afterwards, with SQL to clear it.
              </p>
              <div className="flex gap-2">
                <Button
                  size="sm"
                  disabled={pending}
                  onClick={() => {
                    setAbandoning(false);
                    onAbandon();
                  }}
                >
                  Confirm abandon
                </Button>
                <Button size="sm" variant="outline" onClick={() => setAbandoning(false)}>
                  Keep the merge
                </Button>
              </div>
            </section>
          )}
        </div>
      )}

      {showProgress && (
        <ul className="space-y-0.5">
          {PHASES.filter((p) => steps.some((s) => s.phase === p)).map((p) => {
            const inPhase = steps.filter((s) => s.phase === p);
            const n = inPhase.filter((s) => done.has(s.id)).length;
            return <li key={p}>{`${PHASE_LABEL[p]}: ${n} of ${inPhase.length} steps done`}</li>;
          })}
        </ul>
      )}

      {events.length > 0 && (
        <details className="rounded-lg border px-3 py-2">
          <summary className="cursor-pointer">Merge log ({events.length})</summary>
          <ol aria-label="Merge log" className="space-y-0.5 pt-2 font-mono text-xs [overflow-wrap:anywhere]">
            {events.map((e, i) => (
              <li key={i} className={e.outcome === "failed" ? "text-destructive" : undefined}>
                <span className="text-muted-foreground">{clock(e.at)}</span> {logLine(e)}
              </li>
            ))}
          </ol>
        </details>
      )}
    </div>
  );
}
