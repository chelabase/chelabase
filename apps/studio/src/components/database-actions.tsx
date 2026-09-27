import { useState } from "react";
import type { ConnectionDetails, DatabaseSummary } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import type { ApiClient } from "@/lib/api";

type Panel = "none" | "connection" | "reset" | "delete";

export interface DatabaseActionsProps {
  api: ApiClient;
  database: DatabaseSummary;
  /** Called after a change the database list should reflect (delete). */
  onChanged: () => void;
}

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));

export function DatabaseActions({ api, database, onChanged }: DatabaseActionsProps) {
  const [panel, setPanel] = useState<Panel>("none");
  const [details, setDetails] = useState<ConnectionDetails | null>(null);
  const [showPassword, setShowPassword] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [typedName, setTypedName] = useState("");
  const ready = database.status === "ready";
  const id = database.database_id;

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

  async function reveal(afterReset = false) {
    setBusy(true);
    setError(null);
    try {
      setDetails(await api.getConnection(id));
      setShowPassword(false);
      setNotice(afterReset ? "The password was reset. Update anything using the old one." : null);
      setPanel("connection");
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  }

  async function reset() {
    setBusy(true);
    setError(null);
    try {
      await api.resetPassword(id);
    } catch (err) {
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
          disabled={!ready || busy}
          onClick={() => (panel === "connection" ? open("none") : void reveal())}
        >
          Connection details
        </Button>
        <Button size="sm" variant="outline" disabled={!ready || busy} onClick={() => open("reset")}>
          Reset password
        </Button>
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
      </div>

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
        </div>
      )}

      {panel === "reset" && (
        <div className="space-y-2 rounded-lg border p-3 text-sm">
          <p>Reset the password for {database.name}? The current password stops working for new connections.</p>
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

      {notice && <p className="text-sm text-muted-foreground">{notice}</p>}
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
