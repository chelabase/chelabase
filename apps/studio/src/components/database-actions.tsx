import { useEffect, useRef, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  getGetMeQueryKey,
  useDeleteDatabase,
  useListBranches,
  useResetBranchPassword,
  useResetDatabasePassword,
  type Branch,
  type DatabaseSummary,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { useProjectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { ConnectionDetails } from "./connection-details";

type Panel = "none" | "connection" | "reset" | "delete";

export interface DatabaseActionsProps {
  database: DatabaseSummary;
  /**
   * Act on one branch instead of the database: connection details and reset
   * password go through the branch endpoints and Delete is left to the caller.
   * `reasons` says why each of the two is unavailable (null when available).
   */
  branch?: Branch;
  reasons?: { connect: string | null; resetPassword: string | null };
  /** Ids of the visible texts giving those reasons, for `aria-describedby`. */
  describedBy?: { connect?: string; resetPassword?: string };
  /** More buttons, in the same row. */
  children?: ReactNode;
  /** Shown right under the row of buttons (the reasons some are off). */
  below?: ReactNode;
  /**
   * A branch's connection details to open once, on mount, for this role (null: the owner):
   * a link (a merge's created role) asked for them. The password stays masked until shown.
   */
  openConnection?: { role: string | null };
}

/** The orchestrator's answer when a reset stored the new password but couldn't apply it yet. */
const notApplied = (err: unknown) => err instanceof ApiError && err.code === "not_applied";
/** What a not-applied reset tells the user, instead of both "reset" and the 503. */
const PROTECTED_DB_REASON = "A branch is protected";
const NOT_APPLIED_NOTICE = "The new password is stored but not yet applied; it will be retried.";

export function DatabaseActions({
  database,
  branch,
  reasons,
  describedBy,
  children,
  below,
  openConnection,
}: DatabaseActionsProps) {
  const queryClient = useQueryClient();
  const admin = useProjectCaps().atLeast("admin");
  const resetDatabasePassword = useResetDatabasePassword();
  const resetBranchPassword = useResetBranchPassword();
  const deleteDatabase = useDeleteDatabase({
    mutation: { onSuccess: () => queryClient.invalidateQueries({ queryKey: getGetMeQueryKey() }) },
  });
  const [panel, setPanel] = useState<Panel>("none");
  // What the connection panel opens with; `key` remounts it, so it fetches afresh.
  const [conn, setConn] = useState<{ key: number; role: string | null; notice: string | null; prefix?: string }>({
    key: 0,
    role: null,
    notice: null,
  });
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [typedName, setTypedName] = useState("");
  const ready = database.status === "ready";
  const id = database.database_id;
  const label = branch?.name ?? database.name;
  const connectReason = reasons?.connect ?? null;
  const passwordReason = reasons?.resetPassword ?? null;
  const canConnect = branch ? connectReason === null : ready;
  const canReset = branch ? passwordReason === null : ready;
  // The branches are only listed once the Delete panel is open; its confirm button waits for
  // the list and stays off while any branch is protected.
  const branchList = useListBranches(id, { query: { enabled: !branch && ready && panel === "delete" } });
  const hasProtected = !branch && (branchList.data?.some((b) => b.protected) ?? false);
  const branchesPending = !branch && branchList.data === undefined && !branchList.isError;
  function open(next: Panel) {
    setError(null);
    // Closing the connection panel unmounts it, which drops a revealed password.
    setPanel(panel === next ? "none" : next);
  }

  function showConnection(over: { role?: string | null; notice?: string | null; prefix?: string } = {}) {
    setError(null);
    setConn((c) => ({ key: c.key + 1, role: over.role ?? null, notice: over.notice ?? null, prefix: over.prefix }));
    setPanel("connection");
  }

  // Opened once from a link; later toggles are the user's.
  const opened = useRef(false);
  useEffect(() => {
    if (opened.current || !openConnection || !branch || !canConnect) return;
    opened.current = true;
    showConnection({ role: openConnection.role });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [openConnection, branch, canConnect]);

  async function reset() {
    setBusy(true);
    setError(null);
    try {
      // Nothing cached to invalidate: passwords aren't cached.
      await (branch
        ? resetBranchPassword.mutateAsync({ id, branchId: branch.id })
        : resetDatabasePassword.mutateAsync({ id }));
    } catch (err) {
      if (notApplied(err)) {
        // The new password is stored and shown in the connection panel; it starts working once applied.
        showConnection({ notice: NOT_APPLIED_NOTICE, prefix: NOT_APPLIED_NOTICE });
        setBusy(false);
        return;
      }
      setError(errorText(err));
      setBusy(false);
      return;
    }
    showConnection({ notice: "The password was reset. Update anything using the old one." });
    setBusy(false);
  }

  async function remove() {
    setBusy(true);
    setError(null);
    try {
      // Invalidates getMe: the list shows `deleting` and polls until it's gone.
      await deleteDatabase.mutateAsync({ id });
      setPanel("none");
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  // Connection details, password resets and deleting the database are Admin's (owner actions).
  if (!admin) {
    return children || below ? (
      <div className="space-y-2">
        {children && <div className="flex flex-wrap gap-2">{children}</div>}
        {below}
      </div>
    ) : null;
  }

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap gap-2">
        {/* A database's own connection details live in its Connect panel; branches keep theirs here. */}
        {branch && (
          <Button
            size="sm"
            variant="outline"
            disabled={!canConnect || busy}
            title={connectReason ?? undefined}
            aria-describedby={connectReason ? describedBy?.connect : undefined}
            onClick={() => (panel === "connection" ? open("none") : showConnection())}
          >
            Connection details
          </Button>
        )}
        <Button
          size="sm"
          variant="outline"
          disabled={!canReset || busy}
          title={passwordReason ?? undefined}
          aria-describedby={passwordReason ? describedBy?.resetPassword : undefined}
          onClick={() => open("reset")}
        >
          Reset password
        </Button>
        {!branch && (
          <Button
            size="sm"
            variant="outline"
            disabled={database.status === "deleting" || busy}
            onClick={() => {
              setTypedName("");
              open("delete");
            }}
          >
            Delete
          </Button>
        )}
        {children}
      </div>
      {below}

      {panel === "connection" && (
        <ConnectionDetails
          key={conn.key}
          database={database}
          branch={branch}
          role={conn.role}
          pickers={!!branch}
          notice={conn.notice}
          failurePrefix={conn.prefix}
        />
      )}

      {panel === "reset" && (
        <div className="space-y-2 border p-3 text-sm">
          <p>Reset the password for {label}? The current password stops working for new connections.</p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void reset()}>
              Confirm reset
            </Button>
            <Button size="sm" variant="outline" onClick={() => open("none")}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {panel === "delete" && (
        <div className="space-y-2 border p-3 text-sm">
          <label className="flex flex-col gap-1">
            <span>
              Type the database name (<code>{database.name}</code>) to delete it and all its data.
            </span>
            <input
              value={typedName}
              onChange={(e) => setTypedName(e.target.value)}
              className="h-8 w-64 border border-input bg-background px-2"
            />
          </label>
          {hasProtected && <p className="text-xs text-muted-foreground">{PROTECTED_DB_REASON}</p>}
          {branchesPending && <p className="text-xs text-muted-foreground">Checking branches…</p>}
          <div className="flex gap-2">
            <Button
              size="sm"
              disabled={busy || typedName !== database.name || hasProtected || branchesPending}
              onClick={() => void remove()}
            >
              Delete permanently
            </Button>
            <Button size="sm" variant="outline" onClick={() => open("none")}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {error && (
        <p role="alert" className="text-sm text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}
