import { useEffect, useId, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useRouter } from "@tanstack/react-router";
import {
  ApiError,
  createMerge,
  getGetBranchReviewQueryKey,
  getListMergesQueryKey,
  useAcknowledgeBranchReview,
  useDeleteBranch,
  useGetBranchReview,
  useResetBranchToParent,
  useRestoreBranch,
  useUpdateBranch,
  type Branch,
  type Direction,
  type MergeSummary,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import {
  actionReasons,
  heldReason,
  holdingMergeFor,
  invalidateBranches,
  isTransitional,
  openMergeFor,
  pointToUtc,
  restoreBounds,
  toLocalInput,
  type ActionKey,
} from "@/lib/branches";
import { errorText } from "@/lib/errors";
import { useProjectCaps } from "@/lib/capabilities";
import { mergeErrorText } from "@/lib/merges";
import { BranchRoles } from "./branch-roles";
import { BranchReview, reviewChangedText } from "./branch-review";
import { DatabaseActions } from "./database-actions";
import { MERGE_ROUTE, MergeLink } from "./merges/merge-link";

type Panel = "none" | "protect" | "protection" | "reset-to-parent" | "restore" | "delete";

export interface BranchActionsProps {
  /** The project the database belongs to (for links to its merges). */
  projectId: string;
  databaseId: string;
  databaseName: string;
  /**
   * The main Postgres database (`DatabaseSummary.postgres_database`, the display name sanitized):
   * merges name it, so only its merges block. Unset, any merge of the pair does.
   */
  postgresDatabase?: string | null;
  branch: Branch;
  /** Every branch of the database, for the children rule. */
  branches: Branch[];
  /** Seconds of history kept; bounds the restore picker. */
  historyWindow: number;
  /** The database's merges: an open one between this branch and its parent blocks another. */
  openMerges?: MergeSummary[];
  /** Open the connection details on load, for this role (null: the owner). */
  openConnection?: { role: string | null };
  /** The Roles panel's switch when the caller renders it (the tree gives it its own full-width row); without it, the panel opens below the actions. */
  roles?: { open: boolean; onToggle: () => void };
}

/** A failed action's text, and the merge it names (linked) when it names one. */
type Failure = { text: string; mergeId: string | null };

const RETRY_AFTER_MS = 1000;

export function BranchActions({
  projectId,
  databaseId,
  databaseName,
  postgresDatabase = null,
  branch,
  branches,
  historyWindow,
  openMerges = [],
  openConnection,
  roles,
}: BranchActionsProps) {
  const [localRoles, setLocalRoles] = useState(false);
  const rolesOpen = roles ? roles.open : localRoles;
  const toggleRoles = roles ? roles.onToggle : () => setLocalRoles((v) => !v);
  const queryClient = useQueryClient();
  // Each one refreshes the branch list and the database (returned, so `busy` waits for it).
  const invalidate = { mutation: { onSuccess: (_: unknown, { id }: { id: string }) => invalidateBranches(queryClient, id) } };
  const resetBranch = useResetBranchToParent(invalidate);
  const deleteBranch = useDeleteBranch(invalidate);
  const restoreBranch = useRestoreBranch(invalidate);
  const updateBranch = useUpdateBranch(invalidate);
  const [panel, setPanel] = useState<Panel>("none");
  const [guardLogins, setGuardLogins] = useState(branch.guard_logins);
  const acknowledge = useAcknowledgeBranchReview();
  const [point, setPoint] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<Failure | null>(null);
  // Outside a router (some tests), there is nowhere to go after starting a merge.
  const router = useRouter({ warn: false }) as ReturnType<typeof useRouter> | undefined;
  const [notice, setNotice] = useState<string | null>(null);
  // The notice describes an action in flight: once a refresh shows the branch settled
  // again, it's done (the tree shows how it went).
  const wasTransitional = useRef(isTransitional(branch));
  useEffect(() => {
    const now = isTransitional(branch);
    if (wasTransitional.current && !now) setNotice(null);
    wasTransitional.current = now;
  }, [branch]);
  const mergeDatabase = postgresDatabase ?? undefined;
  const reasons = actionReasons(branch, branches, openMerges, mergeDatabase);
  const parent = branches.find((b) => b.id === branch.parent_branch_id) ?? null;
  const parentName = parent?.name ?? null;
  // Branch routes need Developer on an unprotected branch and Admin on a protected one.
  const caps = useProjectCaps();
  const canChange = (b: Branch) => caps.atLeast(b.protected ? "admin" : "developer");
  // The open merge each merge action's reason names, linked next to the reason.
  const blockingMerge: Partial<Record<ActionKey, MergeSummary>> = {};
  for (const [key, direction] of [["mergeInto", "into_parent"], ["updateFrom", "from_parent"]] as const) {
    const open = openMergeFor(branch, direction, openMerges, mergeDatabase);
    if (open && reasons[key]?.endsWith("is already open")) blockingMerge[key] = open;
  }
  // The merge that holds the branch, linked next to the reason reset, restore and delete show.
  const holding = holdingMergeFor(branch, openMerges);
  const holdReason = heldReason(branch, openMerges);
  // Each distinct reason is shown once, under the buttons, and each disabled button points
  // at its reason with aria-describedby: visible, and read out with the button.
  const baseId = useId();
  const reasonTexts = [...new Set(Object.values(reasons).filter((r): r is string => r !== null))];
  const reasonId = (key: ActionKey) => {
    const reason = reasons[key];
    return reason === null ? undefined : `${baseId}-reason-${reasonTexts.indexOf(reason)}`;
  };
  // The review: loaded for a protected branch (Admin) and when Protect or Protection opens, never otherwise.
  const reviewKey = getGetBranchReviewQueryKey(databaseId, branch.id);
  const reviewWanted = caps.atLeast("admin") && (branch.protected || panel === "protect");
  // The badge's scan runs on the branch's compute: kept 5 minutes and not redone on window focus
  // (opening Protection invalidates it).
  const reviewQuery = useGetBranchReview(databaseId, branch.id, {
    query: { enabled: reviewWanted, retry: false, staleTime: 5 * 60_000, refetchOnWindowFocus: false },
  });
  const review = reviewQuery.data;
  const reviewChanged = branch.protected && !!review && review.hash !== (review.acknowledged_hash ?? null);
  const bounds = restoreBounds(new Date(), historyWindow, branch.history_start);

  function open(next: Panel) {
    if (rolesOpen) toggleRoles();
    setError(null);
    setNotice(null);
    setPoint("");
    setGuardLogins(branch.guard_logins);
    if (next === "protection" && panel !== next) void queryClient.invalidateQueries({ queryKey: reviewKey });
    setPanel(panel === next ? "none" : next);
  }

  async function run(action: () => Promise<string | null>) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      setNotice(await action());
      setPanel("none");
    } catch (err) {
      setError(mergeErrorText(err));
    } finally {
      setBusy(false);
    }
  }

  /** Starts a merge with the parent and opens its page. A pair's merge that just finished gives one quiet retry. */
  async function startMerge(direction: Direction) {
    setBusy(true);
    setError(null);
    setNotice(null);
    setPanel("none");
    const once = () => createMerge(databaseId, branch.id, { direction });
    try {
      let made: MergeSummary;
      try {
        made = await once();
      } catch (err) {
        if (!(err instanceof ApiError && err.code === "merge_busy" && typeof err.extra.merge_id !== "string")) throw err;
        await new Promise((r) => setTimeout(r, RETRY_AFTER_MS));
        made = await once();
      }
      void queryClient.invalidateQueries({ queryKey: getListMergesQueryKey(databaseId) });
      await router?.navigate({ to: MERGE_ROUTE, params: { projectId, databaseId, mergeId: made.id } });
    } catch (err) {
      setError(mergeErrorText(err));
    } finally {
      setBusy(false);
    }
  }

  const setProtected = (value: boolean) =>
    run(async () => {
      await updateBranch.mutateAsync({ id: databaseId, branchId: branch.id, data: { protected: value } });
      return null;
    });
  /** Protects with the review the caller saw; a changed review replaces the list and asks again. */
  async function confirmProtect() {
    if (!review) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await updateBranch.mutateAsync({
        id: databaseId,
        branchId: branch.id,
        data: {
          protected: true,
          review_hash: review.hash,
          ...(guardLogins !== branch.guard_logins ? { guard_logins: guardLogins } : {}),
        },
      });
      void queryClient.invalidateQueries({ queryKey: reviewKey });
      setPanel("none");
    } catch (err) {
      const changed = reviewChangedText(err, queryClient, databaseId, branch.id);
      setError(changed ? { text: changed, mergeId: null } : mergeErrorText(err));
    } finally {
      setBusy(false);
    }
  }
  /** The Protection panel's switch acts at once. */
  async function saveGuardLogins(value: boolean) {
    setBusy(true);
    setError(null);
    try {
      await updateBranch.mutateAsync({ id: databaseId, branchId: branch.id, data: { guard_logins: value } });
    } catch (err) {
      setError(mergeErrorText(err));
    } finally {
      setBusy(false);
    }
  }
  async function acknowledgeReview() {
    if (!review) return;
    setBusy(true);
    setError(null);
    try {
      const done = await acknowledge.mutateAsync({ id: databaseId, branchId: branch.id, data: { hash: review.hash } });
      queryClient.setQueryData(reviewKey, done);
      void invalidateBranches(queryClient, databaseId);
    } catch (err) {
      const changed = reviewChangedText(err, queryClient, databaseId, branch.id);
      setError(changed ? { text: changed, mergeId: null } : mergeErrorText(err));
    } finally {
      setBusy(false);
    }
  }
  const resetToParent = () =>
    run(async () => {
      await resetBranch.mutateAsync({ id: databaseId, branchId: branch.id });
      return null;
    });
  const remove = () =>
    run(async () => {
      await deleteBranch.mutateAsync({ id: databaseId, branchId: branch.id });
      return null;
    });
  function restore() {
    const at = pointToUtc(point, new Date(), historyWindow, branch.history_start);
    if (!at.ok) {
      setError({ text: at.error, mergeId: null });
      return;
    }
    void run(async () => {
      const done = await restoreBranch.mutateAsync({
        id: databaseId,
        branchId: branch.id,
        data: { at: { timestamp: at.iso } },
      });
      return `Restoring ${branch.name}. Its old data is kept in ${done.backup_name}.`;
    });
  }

  const gated = (key: ActionKey) => ({
    disabled: reasons[key] !== null || busy,
    title: reasons[key] ?? undefined,
    "aria-describedby": reasonId(key),
  });

  return (
    <div className="space-y-2">
      <DatabaseActions
        database={{
          database_id: databaseId,
          name: databaseName,
          postgres_database: postgresDatabase,
          status: "ready",
          host: branch.host,
          port: branch.port,
          branch_count: 0,
        }}
        branch={branch}
        openConnection={openConnection}
        reasons={reasons}
        describedBy={{ connect: reasonId("connect"), resetPassword: reasonId("resetPassword") }}
        below={
          reasonTexts.length > 0 && (
            <ul className="space-y-0.5 text-xs text-muted-foreground">
              {reasonTexts.map((reason, i) => {
                const key = (["mergeInto", "updateFrom"] as const).find((k) => reasons[k] === reason && blockingMerge[k]);
                const open = key ? blockingMerge[key] : undefined;
                const holder = reason === holdReason ? holding : null;
                return (
                  <li key={reason}>
                    <span id={`${baseId}-reason-${i}`}>{reason}</span>
                    {open && (
                      <>
                        {" "}
                        <MergeLink projectId={projectId} databaseId={databaseId} mergeId={open.id}>
                          {key === "mergeInto" ? `Open the merge into ${parentName}` : `Open the update from ${parentName}`}
                        </MergeLink>
                      </>
                    )}
                    {holder && (
                      <>
                        {" "}
                        <MergeLink projectId={projectId} databaseId={databaseId} mergeId={holder.id}>
                          Open the merge
                        </MergeLink>
                      </>
                    )}
                  </li>
                );
              })}
            </ul>
          )
        }
      >
        {branch.kind === "normal" && caps.atLeast("admin") &&
          (branch.protected ? (
            <>
              <Button size="sm" variant="outline" disabled={busy} onClick={() => open("protection")}>
                Protection
              </Button>
              <Button size="sm" variant="outline" disabled={busy} onClick={() => void setProtected(false)}>
                Unprotect
              </Button>
            </>
          ) : (
            <Button size="sm" variant="outline" disabled={busy} onClick={() => open("protect")}>
              Protect
            </Button>
          ))}
        {canChange(branch) && (
          <>
            <Button size="sm" variant="outline" {...gated("resetToParent")} onClick={() => open("reset-to-parent")}>
              Reset to parent
            </Button>
            <Button size="sm" variant="outline" {...gated("restore")} onClick={() => open("restore")}>
              Restore
            </Button>
            <Button size="sm" variant="outline" {...gated("delete")} onClick={() => open("delete")}>
              Delete branch
            </Button>
          </>
        )}
        <Button
          size="sm"
          variant="outline"
          disabled={reasons.connect !== null || busy}
          title={reasons.connect ?? undefined}
          aria-describedby={reasonId("connect")}
          aria-expanded={rolesOpen}
          onClick={() => {
            setPanel("none");
            toggleRoles();
          }}
        >
          Roles
        </Button>
        {/* A merge is judged by its target: the parent when merging into it, this branch when updating. */}
        {(parent === null || canChange(parent)) && (
          <Button size="sm" variant="outline" {...gated("mergeInto")} onClick={() => void startMerge("into_parent")}>
            {parentName ? `Merge into ${parentName}` : "Merge into parent"}
          </Button>
        )}
        {canChange(branch) && (
          <Button size="sm" variant="outline" {...gated("updateFrom")} onClick={() => void startMerge("from_parent")}>
            {parentName ? `Update from ${parentName}` : "Update from parent"}
          </Button>
        )}
      </DatabaseActions>

      {!roles && rolesOpen && reasons.connect === null && <BranchRoles projectId={projectId} databaseId={databaseId} branchId={branch.id} />}

      {reviewChanged && (
        <p>
          <span className="border px-2 py-0.5 text-xs">Review changed</span>
        </p>
      )}
      {branch.protected && reviewQuery.isError && (
        <p>
          <span className="border px-2 py-0.5 text-xs">Review unavailable</span>
        </p>
      )}

      {branch.main_database_missing && (
        <p role="status" className="text-sm text-destructive">
          Main database missing: create a database named <code>{databaseName}</code> to bring it back
        </p>
      )}

      {panel === "protect" && (
        <div className="space-y-2 border p-3 text-sm">
          <p>
            Children of a protected branch get new passwords for every role. It can&apos;t be deleted or reset to its
            parent until you unprotect it. Restore stays allowed.
          </p>
          <label className="flex items-center gap-2">
            <Switch aria-label="Guard login roles" checked={guardLogins} onCheckedChange={setGuardLogins} disabled={busy} />
            <span aria-hidden>Guard login roles</span>
          </label>
          {reviewQuery.isError && (
            <p role="alert" className="text-destructive">
              {errorText(reviewQuery.error)}{" "}
              <Button size="xs" variant="outline" onClick={() => void reviewQuery.refetch()}>
                Retry
              </Button>
            </p>
          )}
          {review ? (
            <BranchReview databaseId={databaseId} branchId={branch.id} review={review} canFix />
          ) : (
            !reviewQuery.isError && <p className="text-muted-foreground">Loading the review…</p>
          )}
          <div className="flex gap-2">
            <Button size="sm" disabled={busy || !review} onClick={() => void confirmProtect()}>
              Confirm protect
            </Button>
            <Button size="sm" variant="outline" onClick={() => open("none")}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {panel === "protection" && branch.protected && (
        <div className="space-y-2 border p-3 text-sm">
          <h3 className="font-medium">Protection</h3>
          <label className="flex items-center gap-2">
            <Switch
              aria-label="Guard login roles"
              checked={branch.guard_logins}
              onCheckedChange={(v) => void saveGuardLogins(v)}
              disabled={busy}
            />
            <span aria-hidden>Guard login roles</span>
          </label>
          {reviewQuery.isError && (
            <p role="alert" className="text-destructive">
              {errorText(reviewQuery.error)}{" "}
              <Button size="xs" variant="outline" onClick={() => void reviewQuery.refetch()}>
                Retry
              </Button>
            </p>
          )}
          {review ? (
            <BranchReview databaseId={databaseId} branchId={branch.id} review={review} canFix />
          ) : (
            !reviewQuery.isError && <p className="text-muted-foreground">Loading the review…</p>
          )}
          <div className="flex gap-2">
            <Button size="sm" disabled={busy || !review} onClick={() => void acknowledgeReview()}>
              Acknowledge
            </Button>
            <Button size="sm" variant="outline" onClick={() => open("none")}>
              Close
            </Button>
          </div>
        </div>
      )}

      {panel === "reset-to-parent" && (
        <div className="space-y-2 border p-3 text-sm">
          <p>
            Reset {branch.name} to its parent? Everything written to {branch.name} since it was created is
            discarded, and it gets the parent&apos;s current data.
          </p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void resetToParent()}>
              Confirm reset to parent
            </Button>
            <Button size="sm" variant="outline" onClick={() => open("none")}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {panel === "restore" && (
        <div className="space-y-2 border p-3 text-sm">
          <label className="flex flex-col gap-1">
            <span>Restore to</span>
            <input
              type="datetime-local"
              value={point}
              min={bounds.min}
              max={bounds.max}
              onChange={(e) => setPoint(e.target.value)}
              className="h-8 w-64 border border-input bg-background px-2"
            />
          </label>
          <p className="text-xs text-muted-foreground">
            Your local time, inside the history window and since its history begins (
            {bounds.min.replace("T", " ")} to now:{" "}
            {toLocalInput(new Date()).replace("T", " ")}). The current data is kept as a backup branch.
          </p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={restore}>
              Confirm restore
            </Button>
            <Button size="sm" variant="outline" onClick={() => open("none")}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {panel === "delete" && (
        <div className="space-y-2 border p-3 text-sm">
          <p>
            Delete {branch.name}? Its backups are deleted with it. This can&apos;t be undone.
          </p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void remove()}>
              Confirm delete branch
            </Button>
            <Button size="sm" variant="outline" onClick={() => open("none")}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {notice && (
        <p role="status" className="text-sm text-muted-foreground">
          {notice}
        </p>
      )}
      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error.text}
          {error.mergeId && (
            <>
              {" "}
              <MergeLink projectId={projectId} databaseId={databaseId} mergeId={error.mergeId}>
                Open the merge
              </MergeLink>
            </>
          )}
        </p>
      )}
    </div>
  );
}
