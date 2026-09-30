import { useEffect, useId, useRef, useState } from "react";
import type { BranchSummary } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import type { ApiClient } from "@/lib/api";
import { actionReasons, isTransitional, pointToUtc, restoreBounds, toLocalInput, type ActionKey } from "@/lib/branches";
import { DatabaseActions } from "./database-actions";

type Panel = "none" | "reset-to-parent" | "restore" | "delete";

export interface BranchActionsProps {
  api: ApiClient;
  databaseId: string;
  databaseName: string;
  branch: BranchSummary;
  /** Every branch of the database, for the children rule. */
  branches: BranchSummary[];
  /** Seconds of history kept; bounds the restore picker. */
  historyWindow: number;
  /** Called after a change the branch list should reflect. */
  onChanged: () => void;
}

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));

export function BranchActions({
  api,
  databaseId,
  databaseName,
  branch,
  branches,
  historyWindow,
  onChanged,
}: BranchActionsProps) {
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
      onChanged();
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  }

  const resetToParent = () =>
    run(async () => {
      await api.resetBranchToParent(databaseId, branch.id);
      return null;
    });
  const remove = () =>
    run(async () => {
      await api.deleteBranch(databaseId, branch.id);
      return null;
    });
  function restore() {
    const at = pointToUtc(point, new Date(), historyWindow, branch.history_start);
    if (!at.ok) {
      setError(at.error);
      return;
    }
    void run(async () => {
      const done = await api.restoreBranch(databaseId, branch.id, { at: { timestamp: at.iso } });
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
        api={api}
        database={{
          database_id: databaseId as never,
          name: databaseName,
          status: "ready",
          host: branch.host,
          port: branch.port,
          branch_count: 0,
        }}
        branch={branch}
        reasons={reasons}
        describedBy={{ connect: reasonId("connect"), resetPassword: reasonId("resetPassword") }}
        onChanged={onChanged}
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
        <Button size="sm" variant="outline" {...gated("resetToParent")} onClick={() => open("reset-to-parent")}>
          Reset to parent
        </Button>
        <Button size="sm" variant="outline" {...gated("restore")} onClick={() => open("restore")}>
          Restore
        </Button>
        <Button size="sm" variant="outline" {...gated("delete")} onClick={() => open("delete")}>
          Delete branch
        </Button>
      </DatabaseActions>

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
