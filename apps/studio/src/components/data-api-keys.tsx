import { useId, useState, type FormEvent } from "react";
import type { CreatedDataKey, DataKey } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import type { ApiClient } from "@/lib/api";
import { CopyButton } from "./copy-button";

export interface DataApiKeysProps {
  api: ApiClient;
  databaseId: string;
  branchId: string;
  /** Every live key of the branch; the public key is left to the panel. */
  keys: DataKey[];
  /** Called after a change the key list should reflect. */
  onChanged: () => void;
}

type Pending = { kind: "revoke" | "rotate"; key: DataKey } | null;

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));
const utc = (iso: string) => `${new Date(iso).toISOString().slice(0, 16).replace("T", " ")} UTC`;

export function DataApiKeys({ api, databaseId, branchId, keys, onChanged }: DataApiKeysProps) {
  const [name, setName] = useState("");
  const [role, setRole] = useState("authenticated");
  const [shown, setShown] = useState<CreatedDataKey | null>(null);
  const [pending, setPending] = useState<Pending>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const reasonId = useId();
  const secrets = keys.filter((k) => k.kind === "secret");
  const busyId = useId();
  const nameReason = name.trim() === "" ? "Enter a name for the key." : null;

  async function create(event: FormEvent) {
    event.preventDefault();
    if (nameReason || busy) return;
    setBusy(true);
    setError(null);
    try {
      setShown(await api.createKey(databaseId, branchId, { name: name.trim(), role: role.trim() }));
      setName("");
      onChanged();
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  }

  async function confirm() {
    if (!pending) return;
    setBusy(true);
    setError(null);
    try {
      if (pending.kind === "revoke") {
        await api.revokeKey(databaseId, branchId, pending.key.id);
      } else {
        setShown(await api.rotateKey(databaseId, branchId, pending.key.id));
      }
      setPending(null);
      onChanged();
    } catch (err) {
      setError(message(err));
    } finally {
      setBusy(false);
    }
  }

  const field = "h-8 rounded-lg border border-input bg-background px-2 text-sm";
  return (
    <div className="space-y-2">
      <h3 className="text-sm font-medium">Secret keys</h3>
      {secrets.length === 0 ? (
        <p className="text-sm text-muted-foreground">No secret keys yet.</p>
      ) : (
        <table className="w-full text-left text-sm">
          <thead className="border-b text-muted-foreground">
            <tr>
              <th className="py-1 font-medium">Name</th>
              <th className="py-1 font-medium">Key</th>
              <th className="py-1 font-medium">Role</th>
              <th className="py-1 font-medium">Last used</th>
              <th className="py-1 font-medium">Actions</th>
            </tr>
          </thead>
          <tbody>
            {secrets.map((k) => (
              <tr key={k.id} className="border-b last:border-0">
                <td className="py-1">{k.name}</td>
                <td className="py-1 font-mono">{k.prefix}…</td>
                <td className="py-1">{k.role}</td>
                <td className="py-1">{k.last_used_at ? utc(k.last_used_at) : "never"}</td>
                <td className="py-1">
                  <div className="flex gap-2">
                    <Button
                      size="xs"
                      variant="outline"
                      disabled={busy}
                      aria-describedby={busy ? busyId : undefined}
                      aria-label={`Rotate ${k.name}`}
                      onClick={() => setPending({ kind: "rotate", key: k })}
                    >
                      Rotate
                    </Button>
                    <Button
                      size="xs"
                      variant="outline"
                      disabled={busy}
                      aria-describedby={busy ? busyId : undefined}
                      aria-label={`Revoke ${k.name}`}
                      onClick={() => setPending({ kind: "revoke", key: k })}
                    >
                      Revoke
                    </Button>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {busy && (
        <p id={busyId} className="text-xs text-muted-foreground">
          Another key change is still running.
        </p>
      )}
      {pending && (
        <div className="space-y-2 rounded-lg border p-3 text-sm">
          <p>
            {pending.kind === "revoke"
              ? `Revoke ${pending.key.name}? Anything using it stops working within 30 seconds.`
              : `Rotate ${pending.key.name}? The current key stops working within 30 seconds, and the new one is shown once.`}
          </p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void confirm()}>
              {pending.kind === "revoke" ? "Confirm revoke" : "Confirm rotate"}
            </Button>
            <Button size="sm" variant="outline" onClick={() => setPending(null)}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {shown && (
        <section aria-label="New key" className="space-y-2 rounded-lg border p-3 text-sm">
          <p>Copy this key now. It won&apos;t be shown again.</p>
          <div className="flex flex-wrap items-center gap-2">
            <code className="rounded bg-muted px-1.5 py-0.5 text-xs break-all">{shown.key}</code>
            <CopyButton value={shown.key} label="Copy key" />
          </div>
          <Button size="sm" variant="outline" onClick={() => setShown(null)}>
            Dismiss
          </Button>
        </section>
      )}

      <form onSubmit={create} className="flex flex-wrap items-end gap-3 text-sm">
        <label className="flex flex-col gap-1">
          <span>Key name</span>
          <input value={name} onChange={(e) => setName(e.target.value)} maxLength={63} className={`${field} w-48`} />
        </label>
        <label className="flex flex-col gap-1">
          <span>Role</span>
          <input value={role} onChange={(e) => setRole(e.target.value)} className={`${field} w-40`} />
        </label>
        <Button
          type="submit"
          size="sm"
          disabled={nameReason !== null || busy}
          aria-describedby={nameReason ? reasonId : undefined}
        >
          Create key
        </Button>
      </form>
      {nameReason && (
        <p id={reasonId} className="text-xs text-muted-foreground">
          {nameReason}
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
