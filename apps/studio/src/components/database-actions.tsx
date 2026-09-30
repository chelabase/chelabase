import { useState, type ReactNode } from "react";
import type { BranchSummary, ConnectionDetails, DatabaseSummary } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import { ApiError, type ApiClient } from "@/lib/api";

type Panel = "none" | "connection" | "reset" | "delete";

export interface DatabaseActionsProps {
  api: ApiClient;
  database: DatabaseSummary;
  /** Called after a change the database list should reflect (delete). */
  onChanged: () => void;
  /**
   * Act on one branch instead of the database: connection details and reset
   * password go through the branch endpoints and Delete is left to the caller.
   * `reasons` says why each of the two is unavailable (null when available).
   */
  branch?: BranchSummary;
  reasons?: { connect: string | null; resetPassword: string | null };
  /** Ids of the visible texts giving those reasons, for `aria-describedby`. */
  describedBy?: { connect?: string; resetPassword?: string };
  /** More buttons, in the same row. */
  children?: ReactNode;
  /** Shown right under the row of buttons (the reasons some are off). */
  below?: ReactNode;
}

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));

/** The orchestrator's 503 when a reset stored the new password but couldn't apply it yet. */
const RESET_NOT_APPLIED = "the password was changed but not yet applied; it will be retried";
const notApplied = (err: unknown) =>
  err instanceof ApiError && err.status === 503 && err.message === RESET_NOT_APPLIED;
/** What a not-applied reset tells the user, instead of both "reset" and the 503. */
const NOT_APPLIED_NOTICE = "The new password is stored but not yet applied; it will be retried.";

export function DatabaseActions({
  api,
  database,
  onChanged,
  branch,
  reasons,
  describedBy,
  children,
  below,
}: DatabaseActionsProps) {
  const [panel, setPanel] = useState<Panel>("none");
  const [details, setDetails] = useState<ConnectionDetails | null>(null);
  const [showPassword, setShowPassword] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
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

  function open(next: Panel) {
    setError(null);
    setNotice(null);
    const target = panel === next ? "none" : next;
    // Don't keep a revealed password around once its panel is gone.
    if (target !== "connection") {
      setDetails(null);
      setShowPassword(false);
    }
    setPanel(target);
  }

  /** `pending`: the reset stored the new password but couldn't apply it yet. */
  async function reveal(afterReset = false, pending = false) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      setDetails(await (branch ? api.getBranchConnection(id, branch.id) : api.getConnection(id)));
      setShowPassword(false);
      setNotice(
        pending
          ? NOT_APPLIED_NOTICE
          : afterReset
            ? "The password was reset. Update anything using the old one."
            : null,
      );
      setPanel("connection");
    } catch (err) {
      setError(pending ? `${NOT_APPLIED_NOTICE} (${message(err)})` : message(err));
    } finally {
      setBusy(false);
    }
  }

  async function reset() {
    setBusy(true);
    setError(null);
    try {
      await (branch ? api.resetBranchPassword(id, branch.id) : api.resetPassword(id));
    } catch (err) {
      if (notApplied(err)) {
        // The new password is stored and shown by reveal; it starts working once applied.
        await reveal(true, true);
        return;
      }
      setError(message(err));
      setBusy(false);
      return;
    }
    await reveal(true);
  }

  async function remove() {
    setBusy(true);
    setError(null);
    try {
      await api.deleteDatabase(id);
      setPanel("none");
      onChanged();
    } catch (err) {
      setError(message(err));
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
            <Field
              label="Password"
              field="password"
              value={details.password}
              shown={showPassword ? details.password : "••••••••"}
              onCopy={copy}
            />
            <Field label="HTTP URL" field="HTTP URL" value={details.http_url} shown={details.http_url} onCopy={copy} />
            {showPassword && (
              <Field label="URI" field="URI" value={details.uri} shown={details.uri} onCopy={copy} />
            )}
          </dl>
          <div className="flex flex-wrap gap-2">
            <Button size="sm" variant="outline" onClick={() => setShowPassword((s) => !s)}>
              {showPassword ? "Hide password" : "Show password"}
            </Button>
            <Button size="sm" variant="outline" onClick={() => void copy(details.uri)}>
              Copy URI
            </Button>
          </div>
          <p className="text-xs text-muted-foreground">
            Local dev: pass the proxy&apos;s CA with <code>sslrootcert=.cheladb/neon/proxy-ca.crt</code>. If{" "}
            <code>localtest.me</code> resolves to <code>::1</code> first on your machine, add{" "}
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
          <div className="flex gap-2">
            <Button size="sm" disabled={busy || typedName !== database.name} onClick={() => void remove()}>
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
