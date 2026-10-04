import { useId, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  getBranchConnection,
  getDatabaseConnection,
  getGetMeQueryKey,
  useDeleteDatabase,
  useListBranchDatabases,
  useListBranches,
  useListRoles,
  useResetBranchPassword,
  useResetDatabasePassword,
  type Branch,
  type ConnectionDetails,
  type DatabaseSummary,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { PASSWORD_STATE_TEXT } from "@/lib/password-state";

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
}: DatabaseActionsProps) {
  const queryClient = useQueryClient();
  const resetDatabasePassword = useResetDatabasePassword();
  const resetBranchPassword = useResetBranchPassword();
  const deleteDatabase = useDeleteDatabase({
    mutation: { onSuccess: () => queryClient.invalidateQueries({ queryKey: getGetMeQueryKey() }) },
  });
  const [panel, setPanel] = useState<Panel>("none");
  const [details, setDetails] = useState<ConnectionDetails | null>(null);
  const [showPassword, setShowPassword] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [typedName, setTypedName] = useState("");
  // The login role whose details are shown; null is the owner.
  const [role, setRole] = useState<string | null>(null);
  // The database whose details are shown; null is the branch's main database.
  const [dbName, setDbName] = useState<string | null>(null);
  const pickerId = useId();
  const dbPickerId = useId();
  const ready = database.status === "ready";
  const id = database.database_id;
  const label = branch?.name ?? database.name;
  const connectReason = reasons?.connect ?? null;
  const passwordReason = reasons?.resetPassword ?? null;
  const canConnect = branch ? connectReason === null : ready;
  const canReset = branch ? passwordReason === null : ready;
  const roles = useListRoles(id, branch?.id ?? "", { query: { enabled: !!branch && panel === "connection" } });
  const ownerRole = roles.data?.find((r) => r.origin === "owner");
  const databases = useListBranchDatabases(id, branch?.id ?? "", { query: { enabled: !!branch && panel === "connection" } });
  // Main first, the system database last (labelled).
  const dbRank = (d: { origin: string }) => (d.origin === "main" ? 0 : d.origin === "system" ? 2 : 1);
  const dbRows = [...(databases.data ?? [])].sort((a, b) => dbRank(a) - dbRank(b));
  const mainDb = dbRows.find((d) => d.main)?.name ?? "";
  // The branches are only listed once the Delete panel is open; its confirm button waits for
  // the list and stays off while any branch is protected.
  const branchList = useListBranches(id, { query: { enabled: !branch && ready && panel === "delete" } });
  const hasProtected = !branch && (branchList.data?.some((b) => b.protected) ?? false);
  const branchesPending = !branch && branchList.data === undefined && !branchList.isError;
  function open(next: Panel) {
    setError(null);
    setNotice(null);
    const target = panel === next ? "none" : next;
    // Don't keep a revealed password around once its panel is gone.
    if (target !== "connection") {
      setDetails(null);
      setShowPassword(false);
      setRole(null);
      setDbName(null);
    }
    setPanel(target);
  }

  /** `pending`: the reset stored the new password but couldn't apply it yet. */
  async function reveal(
    afterReset = false,
    pending = false,
    loginRole: string | null = null,
    database: string | null = null,
  ) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      // Secrets stay out of the query cache: the plain functions, into component state.
      setDetails(await (branch
          ? getBranchConnection(id, branch.id, {
              ...(loginRole ? { role: loginRole } : {}),
              ...(database ? { database } : {}),
            })
          : getDatabaseConnection(id)));
      setShowPassword(false);
      setRole(loginRole);
      setDbName(database);
      setNotice(
        pending
          ? NOT_APPLIED_NOTICE
          : afterReset
            ? "The password was reset. Update anything using the old one."
            : null,
      );
      setPanel("connection");
    } catch (err) {
      setError(pending ? `${NOT_APPLIED_NOTICE} (${errorText(err)})` : errorText(err));
    } finally {
      setBusy(false);
    }
  }

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
        // The new password is stored and shown by reveal; it starts working once applied.
        await reveal(true, true);
        return;
      }
      setError(errorText(err));
      setBusy(false);
      return;
    }
    await reveal(true);
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

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setNotice("Copied.");
    } catch {
      setNotice("Copy failed — select and copy manually.");
    }
  }

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap gap-2">
        <Button
          size="sm"
          variant="outline"
          disabled={!canConnect || busy}
          title={connectReason ?? undefined}
          aria-describedby={connectReason ? describedBy?.connect : undefined}
          onClick={() => (panel === "connection" ? open("none") : void reveal())}
        >
          Connection details
        </Button>
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

      {panel === "connection" && details && (
        <div className="space-y-2 rounded-lg border p-3 text-sm">
          {branch && dbRows.length > 0 && (
            <div className="flex items-center gap-2">
              <label htmlFor={dbPickerId}>Database</label>
              <select
                id={dbPickerId}
                value={dbName ?? mainDb}
                disabled={busy}
                onChange={(e) => void reveal(false, false, role, e.target.value === mainDb ? null : e.target.value)}
                className="h-8 rounded-lg border border-input bg-background px-2"
              >
                {dbRows.map((d) => (
                  <option key={d.name} value={d.name}>
                    {d.origin === "system" ? `${d.name} (System)` : d.name}
                  </option>
                ))}
              </select>
            </div>
          )}
          {branch && roles.data && roles.data.length > 1 && (
            <div className="flex items-center gap-2">
              <label htmlFor={pickerId}>Role</label>
              <select
                id={pickerId}
                value={role ?? ownerRole?.name ?? ""}
                disabled={busy}
                onChange={(e) =>
                  void reveal(false, false, e.target.value === ownerRole?.name ? null : e.target.value, dbName)
                }
                className="h-8 rounded-lg border border-input bg-background px-2"
              >
                {roles.data.map((r) => (
                  <option key={r.name} value={r.name}>
                    {r.name}
                  </option>
                ))}
              </select>
            </div>
          )}
          <dl className="grid grid-cols-[auto_1fr_auto] items-center gap-x-4 gap-y-1">
            {(
              [
                ["Host", "host", details.host],
                ["Port", "port", String(details.port)],
                ["Database", "database", details.database],
                ["User", "user", details.user],
              ] as const
            ).map(([label, field, value]) => (
              <Field key={field} label={label} field={field} value={value} shown={value} onCopy={copy} />
            ))}
            {details.password === null ? (
              <>
                <dt className="text-muted-foreground">Password</dt>
                <dd className="col-span-2">{PASSWORD_STATE_TEXT[details.password_state]}</dd>
              </>
            ) : (
              <Field
                label="Password"
                field="password"
                value={details.password}
                shown={showPassword ? details.password : "••••••••"}
                onCopy={copy}
              />
            )}
            <Field label="HTTP URL" field="HTTP URL" value={details.http_url} shown={details.http_url} onCopy={copy} />
            {(showPassword || details.password === null) && (
              <Field label="URI" field="URI" value={details.uri} shown={details.uri} onCopy={copy} />
            )}
          </dl>
          <div className="flex flex-wrap gap-2">
            {details.password !== null && (
              <Button size="sm" variant="outline" onClick={() => setShowPassword((s) => !s)}>
                {showPassword ? "Hide password" : "Show password"}
              </Button>
            )}
            <Button size="sm" variant="outline" onClick={() => void copy(details.uri)}>
              Copy URI
            </Button>
          </div>
          <p className="text-xs text-muted-foreground">
            {details.ca_file && (
              <>
                Local dev: pass the proxy&apos;s CA with <code>sslrootcert={details.ca_file}</code>.{" "}
              </>
            )}
            If <code>localtest.me</code> resolves to <code>::1</code> first on your machine, add{" "}
            <code>hostaddr=127.0.0.1</code>.
          </p>
        </div>
      )}

      {panel === "reset" && (
        <div className="space-y-2 rounded-lg border p-3 text-sm">
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
        <div className="space-y-2 rounded-lg border p-3 text-sm">
          <label className="flex flex-col gap-1">
            <span>
              Type the database name (<code>{database.name}</code>) to delete it and all its data.
            </span>
            <input
              value={typedName}
              onChange={(e) => setTypedName(e.target.value)}
              className="h-8 w-64 rounded-lg border border-input bg-background px-2"
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

/** One connection detail with its own copy button. */
function Field({
  label,
  field,
  value,
  shown,
  onCopy,
}: {
  label: string;
  field: string;
  value: string;
  shown: string;
  onCopy: (text: string) => Promise<void>;
}) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className="font-mono break-all">{shown}</dd>
      <dd>
        {field !== "URI" && (
          <Button size="xs" variant="ghost" aria-label={`Copy ${field}`} onClick={() => void onCopy(value)}>
            Copy
          </Button>
        )}
      </dd>
    </>
  );
}
