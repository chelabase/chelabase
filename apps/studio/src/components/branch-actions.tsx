import { useEffect, useId, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useDeleteBranch, useResetBranchToParent, useRestoreBranch, useUpdateBranch, type Branch } from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import {
  actionReasons,
  invalidateBranches,
  isTransitional,
  pointToUtc,
  restoreBounds,
  toLocalInput,
  type ActionKey,
} from "@/lib/branches";
import { errorText } from "@/lib/errors";
import { BranchRoles } from "./branch-roles";
import { DatabaseActions } from "./database-actions";

type Panel = "none" | "protect" | "reset-to-parent" | "restore" | "delete" | "roles";

export interface BranchActionsProps {
  databaseId: string;
  databaseName: string;
  branch: Branch;
  /** Every branch of the database, for the children rule. */
  branches: Branch[];
  /** Seconds of history kept; bounds the restore picker. */
  historyWindow: number;
}

export function BranchActions({ databaseId, databaseName, branch, branches, historyWindow }: BranchActionsProps) {
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
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // The notice describes an action in flight: once a refresh shows the branch settled
  // again, it's done (the tree shows how it went).
  const wasTransitional = useRef(isTransitional(branch));
  useEffect(() => {
    const now = isTransitional(branch);
    if (wasTransitional.current && !now) setNotice(null);
    wasTransitional.current = now;
  }, [branch]);
  const reasons = actionReasons(branch, branches);
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
      setError(errorText(err));
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
      setError(at.error);
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
          status: "ready",
          host: branch.host,
          port: branch.port,
          branch_count: 0,
        }}
        branch={branch}
        reasons={reasons}
        describedBy={{ connect: reasonId("connect"), resetPassword: reasonId("resetPassword") }}
        below={
          reasonTexts.length > 0 && (
            <ul className="space-y-0.5 text-xs text-muted-foreground">
              {reasonTexts.map((reason, i) => (
                <li key={reason} id={`${baseId}-reason-${i}`}>
                  {reason}
                </li>
              ))}
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
          {error}
        </p>
      )}
    </div>
  );
}
