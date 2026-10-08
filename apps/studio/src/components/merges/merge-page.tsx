import { useId, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import {
  ApiError,
  abandonMerge,
  applyMerge,
  cancelMerge,
  finishMerge,
  getGetMergeQueryKey,
  getListBranchesQueryKey,
  getListMergesQueryKey,
  recomputeMerge,
  rehearseMerge,
  retryMerge,
  updateMerge,
  useGetMerge,
  useListBranches,
  type AfterMerge,
  type Merge,
  type MergeDetail,
  type MergeSummary,
  type UpdateMergeRequest,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import {
  MERGE_STATUS_LABEL,
  PHASE_LABEL,
  baseUnavailable,
  explainMergeError,
  isFinishPending,
  isMergeBusy,
  isMergeCancellable,
  isMergeEditable,
  mergeErrorText,
  mergeTitle,
  readAnalysis,
  readPlan,
  readRehearsal,
  readResult,
  utcMinute,
  type Phase,
} from "@/lib/merges";
import type { PglsClient } from "@/lib/pgls/client";
import { pollWhile } from "@/lib/query";
import { MergeApply } from "./merge-apply";
import { MergeChanges } from "./merge-changes";
import { MergeConflicts } from "./merge-conflicts";
import { ErrorBody } from "./merge-error";
import { MergeFinish } from "./merge-finish";
import { MergeLink } from "./merge-link";
import { MergePlan } from "./merge-plan";
import { MergeRehearsal } from "./merge-rehearsal";

/** What a section saves: any of the draft's fields; the page adds the `version` it last read. */
export type MergePatch = Omit<UpdateMergeRequest, "version">;
/** Saves a patch; false when it was refused (the page shows why). */
export type SaveMerge = (patch: MergePatch) => Promise<boolean>;

export interface MergePageProps {
  projectId: string;
  databaseId: string;
  mergeId: string;
  /** Starts the pgls worker for own-SQL completion (tests inject a fake). */
  createClient?: () => Promise<PglsClient>;
}

type Failure = { text: string; mergeId: string | null };

const BUSY_TEXT: Partial<Record<Merge["status"], (target: string) => string>> = {
  computing: () => "Reading both schemas and the branch point, then planning…",
  rehearsing: (t) => `Rehearsing on a throwaway branch of ${t}…`,
  applying: (t) => `Applying to ${t}…`,
  online: (t) => `Building indexes and validating constraints on ${t}, one statement at a time…`,
  finishing: (t) => `Checking ${t} against the plan…`,
};

const isMergeRecord = (v: unknown): v is Merge =>
  typeof v === "object" && v !== null && typeof (v as Merge).id === "string" && typeof (v as Merge).version === "number";

/** One merge: what changes, the conflicts, the plan, the rehearsal, the apply and the finish. */
export function MergePage({ projectId, databaseId, mergeId, createClient }: MergePageProps) {
  const queryClient = useQueryClient();
  const key = getGetMergeQueryKey(databaseId, mergeId);
  const detail = useGetMerge(databaseId, mergeId, {
    // A worker holds the merge, or an after-merge choice run by hand is on its way.
    query: { refetchInterval: pollWhile<MergeDetail>((d) => isMergeBusy(d.merge.status) || isFinishPending(d.merge)) },
  });
  const branches = useListBranches(databaseId);
  const [pending, setPending] = useState(false);
  const [failure, setFailure] = useState<Failure | null>(null);

  const merge = detail.data?.merge;
  const name = (id: string) =>
    branches.data === undefined ? "…" : (branches.data.find((b) => b.id === id)?.name ?? "a deleted branch");
  const target = merge ? branches.data?.find((b) => b.id === merge.target_branch_id) : undefined;

  /** The merge's fields from an action's answer, at once; the refetch brings the rest. */
  function patchCache(next: Partial<Merge>) {
    queryClient.setQueryData<MergeDetail>(key, (old) => (old ? { ...old, merge: { ...old.merge, ...next } } : old));
  }
  const refresh = () =>
    Promise.all([
      queryClient.invalidateQueries({ queryKey: key }),
      queryClient.invalidateQueries({ queryKey: getListMergesQueryKey(databaseId) }),
    ]);

  /** Shows a refusal; when someone else saved first, the page takes their version (the sections re-read it). */
  function fail(err: unknown) {
    if (err instanceof ApiError && err.code === "merge_version_conflict" && isMergeRecord(err.extra.current)) {
      patchCache(err.extra.current);
      void refresh();
    }
    setFailure(mergeErrorText(err));
  }

  const save: SaveMerge = async (patch) => {
    if (!merge) return false;
    setPending(true);
    setFailure(null);
    try {
      const saved = await updateMerge(databaseId, mergeId, { ...patch, version: merge.version });
      patchCache(saved);
      void refresh();
      return true;
    } catch (err) {
      fail(err);
      return false;
    } finally {
      setPending(false);
    }
  };

  async function act(action: () => Promise<MergeSummary>) {
    setPending(true);
    setFailure(null);
    try {
      patchCache(await action());
      void refresh();
    } catch (err) {
      fail(err);
    } finally {
      setPending(false);
    }
  }

  /**
   * Applies; "confirm" when the API says the target is protected after all (the branch list was
   * read before it was protected): the list is reloaded and the confirm opens.
   */
  async function apply(confirm: boolean): Promise<"confirm" | undefined> {
    setPending(true);
    setFailure(null);
    try {
      // The version the user saw: a save since (the after-merge choice, say) is applied as seen.
      patchCache(await applyMerge(databaseId, mergeId, { ...(confirm ? { confirm: true } : {}), version: merge?.version }));
      void refresh();
      return undefined;
    } catch (err) {
      if (err instanceof ApiError && err.code === "merge_confirm_required") {
        void queryClient.invalidateQueries({ queryKey: getListBranchesQueryKey(databaseId) });
        return "confirm";
      }
      fail(err);
      return undefined;
    } finally {
      setPending(false);
    }
  }

  if (detail.data === undefined || merge === undefined) {
    return (
      <div className="mx-auto max-w-4xl space-y-4 p-8">
        <BackLink projectId={projectId} />
        {detail.isError ? (
          <div className="space-y-3">
            <p role="alert" className="text-destructive">
              Couldn&apos;t load the merge: {errorText(detail.error)}
            </p>
            <Button onClick={() => void detail.refetch()}>Retry</Button>
          </div>
        ) : (
          <p className="text-muted-foreground">Loading the merge…</p>
        )}
      </div>
    );
  }

  const sourceName = name(merge.source_branch_id);
  const targetName = name(merge.target_branch_id);
  const analysis = readAnalysis(merge.analysis);
  const plan = readPlan(merge.plan);
  const editable = isMergeEditable(merge.status) && !pending;
  const busyText = BUSY_TEXT[merge.status]?.(targetName);
  const recomputable =
    merge.status === "draft" || merge.status === "stale" || (merge.status === "failed" && merge.failed_phase !== "online");
  const gone = baseUnavailable(merge);
  const goneReason = gone ? explainMergeError(merge.error)?.message || null : null;
  const conflicts = analysis?.conflicts ?? [];
  // A section's unsaved edits start again from the server's only when what the section shows from
  // the server changes (its own save, or someone else's), not on every save of another section.
  const conflictsKey = JSON.stringify([merge.resolutions, conflicts.map((c) => c.id)]);
  // The Plan section follows its acknowledgements and lock timeout itself, so their saves keep its
  // unsaved step SQL; a change to the steps starts it again.
  const planKey = JSON.stringify(merge.steps);
  const finishKey = JSON.stringify([merge.options.after ?? null, merge.status === "applied_with_drift"]);

  return (
    <div className="mx-auto max-w-4xl space-y-8 p-8">
      <div className="space-y-2">
        <BackLink projectId={projectId} />
        <h1 className="text-2xl font-semibold">{mergeTitle(merge.direction, sourceName, targetName)}</h1>
        <p className="text-sm text-muted-foreground">
          <span data-testid="merge-status" className="text-foreground">
            {MERGE_STATUS_LABEL[merge.status]}
          </span>{" "}
          · database <code>{merge.database}</code> · started {utcMinute(merge.created_at)}
          {target?.protected && <> · {targetName} is protected</>}
        </p>
        {busyText && (
          <p role="status" className="text-sm">
            {busyText}
          </p>
        )}
        <EndNotice merge={merge} targetName={targetName} />
        <div className="flex flex-wrap gap-2 pt-1">
          {recomputable && !gone && (
            <Button size="sm" variant="outline" disabled={pending} onClick={() => void act(() => recomputeMerge(databaseId, mergeId, {}))}>
              Recompute
            </Button>
          )}
          {isMergeCancellable(merge.status) && (
            <Button size="sm" variant="outline" disabled={pending} onClick={() => void act(() => cancelMerge(databaseId, mergeId))}>
              Cancel merge
            </Button>
          )}
        </div>
        {failure && (
          <p role="alert" className="text-sm text-destructive">
            {failure.text}
            {failure.mergeId && (
              <>
                {" "}
                <MergeLink projectId={projectId} databaseId={databaseId} mergeId={failure.mergeId}>
                  Open the merge
                </MergeLink>
              </>
            )}
          </p>
        )}
      </div>

      {gone && (
        <section aria-label="The branch point's schema is gone" className="space-y-2 rounded-lg border border-destructive/60 p-3 text-sm">
          <p className="font-medium text-destructive">The branch point&apos;s schema is gone.</p>
          <p>
            It can&apos;t be read any more: it wasn&apos;t saved, or it has aged out of the history window, or what it
            was saved from is gone. Without it, the merge can&apos;t tell which side changed what.
          </p>
          {goneReason && (
            <p>
              Why: <span className="font-mono text-xs">{goneReason}</span>
            </p>
          )}
          <p>
            Re-baseline uses {targetName}&apos;s current schema as the base instead. Every difference then counts as a change
            from {sourceName}, so {targetName}&apos;s own changes since the branch point will show as things to undo. Renames
            show as a drop and a create. Drops still need acknowledging one by one before the apply.
          </p>
          <Button
            size="sm"
            variant="outline"
            disabled={pending || !recomputable}
            onClick={() => void act(() => recomputeMerge(databaseId, mergeId, { rebaseline: true }))}
          >
            Re-baseline
          </Button>
        </section>
      )}
      {merge.rebaselined && (
        <p role="note" className="rounded-lg border border-destructive/60 p-3 text-sm">
          This merge was re-baselined: {targetName}&apos;s current schema stood in for the branch point, so{" "}
          {targetName}&apos;s own changes since then show as things to undo. Check every drop before you acknowledge it.
        </p>
      )}
      {merge.error && !gone && showsErrorPanel(merge) && <ErrorPanel merge={merge} />}

      <Section title="Changes">
        {analysis ? (
          <MergeChanges analysis={analysis} sourceName={sourceName} targetName={targetName} />
        ) : (
          <p className="text-sm text-muted-foreground">Not computed yet.</p>
        )}
      </Section>

      {analysis && conflicts.length > 0 && (
        <Section title={`Conflicts (${conflicts.length})`}>
          <MergeConflicts
            key={conflictsKey}
            databaseId={databaseId}
            merge={merge}
            analysis={analysis}
            sourceName={sourceName}
            targetName={targetName}
            editable={editable}
            save={save}
            createClient={createClient}
          />
        </Section>
      )}

      <Section title="Plan">
        <MergePlan key={planKey} databaseId={databaseId} targetName={targetName} merge={merge} plan={plan} editable={editable} save={save} />
      </Section>

      <Section title="Rehearsal">
        <MergeRehearsal
          merge={merge}
          plan={plan}
          targetName={targetName}
          pending={pending}
          onRehearse={() => void act(() => rehearseMerge(databaseId, mergeId, { version: merge.version }))}
        />
      </Section>

      <Section title="Apply">
        <MergeApply
          merge={merge}
          plan={plan}
          events={detail.data.events}
          targetName={targetName}
          targetProtected={target?.protected ?? false}
          branchesLoaded={branches.data !== undefined}
          pending={pending}
          onApply={apply}
          onRetry={() => void act(() => retryMerge(databaseId, mergeId))}
          onAbandon={() => void act(() => abandonMerge(databaseId, mergeId))}
        />
      </Section>

      <Section title="Finish">
        <MergeFinish
          key={finishKey}
          projectId={projectId}
          databaseId={databaseId}
          merge={merge}
          hasPlan={plan !== null}
          sourceName={sourceName}
          targetName={targetName}
          editable={editable}
          pending={pending}
          save={save}
          onFinish={(after: AfterMerge) => void act(() => finishMerge(databaseId, mergeId, { after }))}
        />
      </Section>
    </div>
  );
}

function BackLink({ projectId }: { projectId: string }) {
  return (
    <p>
      <Link
        to="/app/projects/$projectId"
        params={{ projectId }}
        className="text-sm text-muted-foreground underline-offset-4 hover:underline"
      >
        Back to the project
      </Link>
    </p>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  const id = useId();
  // The region's name is the title without its count, so it stays stable.
  return (
    <section aria-labelledby={id} className="space-y-3 border-t pt-6">
      <h2 className="text-lg font-semibold">
        <span id={id}>{title.replace(/ \(\d+\)$/, "")}</span>
        {/\(\d+\)$/.test(title) && <span className="text-muted-foreground"> {title.match(/\(\d+\)$/)![0]}</span>}
      </h2>
      {children}
    </section>
  );
}

/**
 * Whether the page shows the error panel: not when another part of the page already tells the
 * error (a failed merge's end notice, a stale merge's notice with the worker's sentence, or the
 * Rehearsal section with the failed step and Postgres's text).
 */
function showsErrorPanel(merge: Merge): boolean {
  if (merge.status === "failed") return false;
  const kind = explainMergeError(merge.error)?.kind;
  if (merge.status === "stale" && kind === "stale") return false;
  if (kind === "rehearsal_failed" && readRehearsal(merge.rehearsal)?.failed_step) return false;
  return true;
}

/** The notice for a merge's `error` on a merge that hasn't ended: its heading follows the status and the error. */
function ErrorPanel({ merge }: { merge: Merge }) {
  const info = explainMergeError(merge.error, merge.status);
  const draft = merge.status === "draft" || merge.status === "computing";
  const heading =
    info?.kind === "resolution_refused"
      ? "A saved choice was refused. Its conflict is open again: choose again and save."
      : info?.kind === "apply_not_started"
        ? "The apply didn't start."
        : info?.kind === "interrupted" || merge.status === "rehearsed"
          ? "The apply was interrupted. The merge is rehearsed again: apply it once more."
          : draft && info?.kind === "rehearsal_not_run"
          ? "The rehearsal couldn't run."
          : draft && info?.kind === "rehearsal_unread"
            ? "The rehearsal ran, but its result couldn't be read. Rehearse again."
            : draft && info?.kind === "rehearsal_failed"
              ? "The rehearsal failed."
              : draft
                ? "The last compute couldn't finish the plan."
                : "The merge reported a problem.";
  return (
    <div role="alert" className="space-y-1 rounded-lg border border-destructive/60 p-3 text-sm">
      <p className="font-medium text-destructive">{heading}</p>
      {merge.error && <ErrorBody error={merge.error} status={merge.status} />}
    </div>
  );
}

/** How an ended merge ended, in plain words. */
function EndNotice({ merge, targetName }: { merge: Merge; targetName: string }) {
  switch (merge.status) {
    case "done":
      return <p className="text-sm">Done: {targetName} has the merged schema.</p>;
    case "applied_with_drift":
      return (
        <p className="text-sm text-destructive">
          Applied, but {targetName}&apos;s schema differs from what the merge expected. The differences are under Finish.
        </p>
      );
    case "stale": {
      const info = explainMergeError(merge.error);
      return (
        <p className="text-sm">
          {info?.kind === "stale"
            ? `${info.message[0].toUpperCase()}${info.message.slice(1)}`
            : "A schema changed since the merge was computed, so the plan's transaction wasn't applied. Recompute to plan again from the schemas as they are now."}
        </p>
      );
    }
    case "cancelled":
      return readResult(merge.result)?.leftovers != null ? (
        <p className="text-sm">
          Abandoned half applied. {targetName} keeps the plan&apos;s transaction and whatever the online steps left; both
          branches are free again. What was left is listed under Finish.
        </p>
      ) : (
        <p className="text-sm text-muted-foreground">Cancelled. Nothing was applied.</p>
      );
    case "failed": {
      if (merge.failed_phase === "online") return null;
      const info = explainMergeError(merge.error);
      if (merge.failed_phase !== null && merge.failed_phase in PHASE_LABEL) {
        return (
          <p className="text-sm text-destructive">
            The merge failed in {PHASE_LABEL[merge.failed_phase as Phase].toLowerCase()}. Retry the apply under Apply, or
            recompute to plan again.
          </p>
        );
      }
      return (
        <div role="alert" className="space-y-1 text-sm">
          <p className="text-destructive">
            {info?.kind === "target_changed"
              ? "The target changed under the merge."
              : info?.kind === "target_gone"
                ? "The merge's target is gone."
                : "The merge failed. Recompute to try again."}
          </p>
          {merge.error && <ErrorBody error={merge.error} />}
        </div>
      );
    }
    default:
      return null;
  }
}
