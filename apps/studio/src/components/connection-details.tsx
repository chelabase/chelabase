import { useEffect, useId, useRef, useState } from "react";
import {
  getBranchConnection,
  getDatabaseConnection,
  useListBranchDatabases,
  useListRoles,
  type Branch,
  type ConnectionDetails as ConnectionInfo,
  type DatabaseSummary,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { PASSWORD_STATE_TEXT } from "@/lib/password-state";

export interface ConnectionDetailsProps {
  database: DatabaseSummary;
  /** The branch whose connection is shown; without one, the database's own. */
  branch?: Branch;
  /** The Postgres database on that branch (null: the branch's main database). */
  pgDatabase?: string | null;
  /** The login role to connect as (null: the owner). */
  role?: string | null;
  /**
   * Show the Database and Role pickers (a branch row's panel). Without them the caller owns
   * both choices and a change of `pgDatabase` re-fetches.
   */
  pickers?: boolean;
  /** Shown after the first successful load (a reset's outcome). */
  notice?: string | null;
  /** Prefixed to a failed load's error (a reset that is stored but not yet applied). */
  failurePrefix?: string;
}

/**
 * The connection details of one branch's Postgres database, fetched on mount and on every change
 * of branch, database or role. The password stays masked until Show and is masked again on each
 * fetch. Secrets stay out of the query cache: the plain functions, into component state.
 */
export function ConnectionDetails({
  database,
  branch,
  pgDatabase = null,
  role: initialRole = null,
  pickers = false,
  notice: initialNotice = null,
  failurePrefix,
}: ConnectionDetailsProps) {
  const id = database.database_id;
  const branchId = branch?.id;
  const [details, setDetails] = useState<ConnectionInfo | null>(null);
  const [showPassword, setShowPassword] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // The login role whose details are shown; null is the owner.
  const [role, setRole] = useState<string | null>(initialRole);
  // With pickers, the database is ours to change; otherwise the caller's.
  const [pickedDb, setPickedDb] = useState<string | null>(pgDatabase);
  const dbName = pickers ? pickedDb : pgDatabase;
  const pendingNotice = useRef(initialNotice);
  const pickerId = useId();
  const dbPickerId = useId();

  const roles = useListRoles(id, branchId ?? "", { query: { enabled: !!branch && pickers } });
  const ownerRole = roles.data?.find((r) => r.origin === "owner");
  const databases = useListBranchDatabases(id, branchId ?? "", { query: { enabled: !!branch && pickers } });
  // Main first, the system database last (labelled).
  const dbRank = (d: { origin: string }) => (d.origin === "main" ? 0 : d.origin === "system" ? 2 : 1);
  const dbRows = [...(databases.data ?? [])].sort((a, b) => dbRank(a) - dbRank(b));
  const mainDb = dbRows.find((d) => d.main)?.name ?? "";

  useEffect(() => {
    let live = true;
    setBusy(true);
    setError(null);
    setNotice(null);
    setShowPassword(false);
    setDetails(null);
    void (async () => {
      try {
        const next = await (branchId
          ? getBranchConnection(id, branchId, {
              ...(role ? { role } : {}),
              ...(dbName ? { database: dbName } : {}),
            })
          : getDatabaseConnection(id));
        if (!live) return;
        setDetails(next);
        setNotice(pendingNotice.current);
      } catch (err) {
        if (live) setError(failurePrefix ? `${failurePrefix} (${errorText(err)})` : errorText(err));
      } finally {
        pendingNotice.current = null;
        if (live) setBusy(false);
      }
    })();
    return () => {
      live = false;
    };
    // failurePrefix only matters to the first load.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, branchId, role, dbName]);

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      setNotice("Copied.");
    } catch {
      setNotice("Copy failed — select and copy manually.");
    }
  }

  return (
    <div className="space-y-2 border p-3 text-sm">
      {pickers && branch && dbRows.length > 0 && (
        <div className="flex items-center gap-2">
          <label htmlFor={dbPickerId}>Database</label>
          <select
            id={dbPickerId}
            value={pickedDb ?? mainDb}
            disabled={busy}
            onChange={(e) => setPickedDb(e.target.value === mainDb ? null : e.target.value)}
            className="h-8 border border-input bg-background px-2"
          >
            {dbRows.map((d) => (
              <option key={d.name} value={d.name}>
                {d.origin === "system" ? `${d.name} (System)` : d.name}
              </option>
            ))}
          </select>
        </div>
      )}
      {pickers && branch && roles.data && roles.data.length > 1 && (
        <div className="flex items-center gap-2">
          <label htmlFor={pickerId}>Role</label>
          <select
            id={pickerId}
            value={role ?? ownerRole?.name ?? ""}
            disabled={busy}
            onChange={(e) => setRole(e.target.value === ownerRole?.name ? null : e.target.value)}
            className="h-8 border border-input bg-background px-2"
          >
            {roles.data.map((r) => (
              <option key={r.name} value={r.name}>
                {r.name}
              </option>
            ))}
          </select>
        </div>
      )}
      {details && (
        <>
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
                CA file: pass the proxy&apos;s CA with <code>sslrootcert={details.ca_file}</code>.
                Self-hosted, fetch it with <code>docker compose cp proxy:/tls/ca.crt {details.ca_file}</code> (install
                guide, &ldquo;Connect to databases&rdquo;).{" "}
              </>
            )}
            If <code>localtest.me</code> resolves to <code>::1</code> first on your machine, add{" "}
            <code>hostaddr=127.0.0.1</code>.
          </p>
        </>
      )}
      {busy && !details && <p className="text-muted-foreground">Loading connection details…</p>}
      {notice && (
        <p role="status" className="text-muted-foreground">
          {notice}
        </p>
      )}
      {error && (
        <p role="alert" className="text-destructive">
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
