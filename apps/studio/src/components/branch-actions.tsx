import { useEffect, useId, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useRouter } from "@tanstack/react-router";
import {
  ApiError,
  createMerge,
  getListMergesQueryKey,
  useDeleteBranch,
  useResetBranchToParent,
  useRestoreBranch,
  useUpdateBranch,
  type Branch,
  type Direction,
  type MergeSummary,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
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
import { mergeErrorText } from "@/lib/merges";
import { BranchRoles } from "./branch-roles";
import { DatabaseActions } from "./database-actions";
import { MERGE_ROUTE, MergeLink } from "./merges/merge-link";

type Panel = "none" | "protect" | "reset-to-parent" | "restore" | "delete" | "roles";

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
}: BranchActionsProps) {
  const queryClient = useQueryClient();
  // Each one refreshes the branch list and the database (returned, so `busy` waits for it).
  const invalidate = { mutation: { onSuccess: (_: unknown, { id }: { id: string }) => invalidateBranches(queryClient, id) } };
  const resetBranch = useResetBranchToParent(invalidate);
  const deleteBranch = useDeleteBranch(invalidate);
  const restoreBranch = useRestoreBranch(invalidate);
  const updateBranch = useUpdateBranch(invalidate);
  const [panel, setPanel] = useState<Panel>("none");
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
  const parentName = branches.find((b) => b.id === branch.parent_branch_id)?.name ?? null;
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
  const bounds = restoreBounds(new Date(), historyWindow, branch.history_start);

  function open(next: Panel) {
    setError(null);
    setNotice(null);
    setPoint("");
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
        {branch.kind === "normal" &&
          (branch.protected ? (
            <Button size="sm" variant="outline" disabled={busy} onClick={() => void setProtected(false)}>
              Unprotect
            </Button>
          ) : (
            <Button size="sm" variant="outline" disabled={busy} onClick={() => open("protect")}>
              Protect
            </Button>
          ))}
        <Button size="sm" variant="outline" {...gated("resetToParent")} onClick={() => open("reset-to-parent")}>
          Reset to parent
        </Button>
        <Button size="sm" variant="outline" {...gated("restore")} onClick={() => open("restore")}>
          Restore
        </Button>
        <Button size="sm" variant="outline" {...gated("delete")} onClick={() => open("delete")}>
          Delete branch
        </Button>
        <Button
          size="sm"
          variant="outline"
          disabled={reasons.connect !== null || busy}
          title={reasons.connect ?? undefined}
          aria-describedby={reasonId("connect")}
          onClick={() => open("roles")}
        >
          Roles
        </Button>
        <Button size="sm" variant="outline" {...gated("mergeInto")} onClick={() => void startMerge("into_parent")}>
          {parentName ? `Merge into ${parentName}` : "Merge into parent"}
        </Button>
        <Button size="sm" variant="outline" {...gated("updateFrom")} onClick={() => void startMerge("from_parent")}>
          {parentName ? `Update from ${parentName}` : "Update from parent"}
        </Button>
      </DatabaseActions>

      {panel === "roles" && reasons.connect === null && <BranchRoles databaseId={databaseId} branchId={branch.id} />}

      {branch.main_database_missing && (
        <p role="status" className="text-sm text-destructive">
          Main database missing: create a database named <code>{databaseName}</code> to bring it back
        </p>
      )}

      {panel === "protect" && (
        <div className="space-y-2 rounded-lg border p-3 text-sm">
          <p>
            Children of a protected branch get new passwords for every role. It can&apos;t be deleted or reset to its
            parent until you unprotect it. Restore stays allowed.
          </p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void setProtected(true)}>
              Confirm protect
            </Button>
            <Button size="sm" variant="outline" onClick={() => open("none")}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {panel === "reset-to-parent" && (
        <div className="space-y-2 rounded-lg border p-3 text-sm">
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
        <div className="space-y-2 rounded-lg border p-3 text-sm">
          <label className="flex flex-col gap-1">
            <span>Restore to</span>
            <input
              type="datetime-local"
              value={point}
              min={bounds.min}
              max={bounds.max}
              onChange={(e) => setPoint(e.target.value)}
              className="h-8 w-64 rounded-lg border border-input bg-background px-2"
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
        <div className="space-y-2 rounded-lg border p-3 text-sm">
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
