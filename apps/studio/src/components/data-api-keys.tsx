import { useId, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  createKey,
  getListKeysQueryKey,
  rotateKey,
  useRevokeKey,
  type BranchDatabaseView,
  type CreatedDataKey,
  type DataKey,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { CopyButton } from "./copy-button";
import { DatabaseSelector, KeyDatabases, selectionReason } from "./data-api-databases";

export interface DataApiKeysProps {
  databaseId: string;
  branchId: string;
  /** Every live key of the branch; the public key is left to the panel. */
  keys: DataKey[];
  /** The branch's databases, for the allow-lists. */
  databases: BranchDatabaseView[];
}

type Pending = { kind: "revoke" | "rotate"; key: DataKey } | null;

const utc = (iso: string) => `${new Date(iso).toISOString().slice(0, 16).replace("T", " ")} UTC`;

export function DataApiKeys({ databaseId, branchId, keys, databases }: DataApiKeysProps) {
  const queryClient = useQueryClient();
  const refreshKeys = (id: string, branch: string) =>
    queryClient.invalidateQueries({ queryKey: getListKeysQueryKey(id, branch) });
  // Revoking returns no secret, so it can be an ordinary mutation.
  const revoke = useRevokeKey({ mutation: { onSuccess: (_, { id, branchId: branch }) => refreshKeys(id, branch) } });
  const [name, setName] = useState("");
  const [role, setRole] = useState("authenticated");
  // `null` is every served database.
  const [allowed, setAllowed] = useState<string[] | null>(null);
  const [shown, setShown] = useState<CreatedDataKey | null>(null);
  const [pending, setPending] = useState<Pending>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const reasonId = useId();
  const secrets = keys.filter((k) => k.kind === "secret");
  const busyId = useId();
  const nameReason = name.trim() === "" ? "Enter a name for the key." : selectionReason(allowed);

  async function create(event: FormEvent) {
    event.preventDefault();
    if (nameReason || busy) return;
    setBusy(true);
    setError(null);
    try {
      // A new key is a secret: the plain function keeps it out of the mutation cache; it
      // lives only in this component's state.
      setShown(await createKey(databaseId, branchId, {
          name: name.trim(),
          role: role.trim(),
          ...(allowed ? { databases: allowed } : {}),
        }));
      setName("");
      setAllowed(null);
      await refreshKeys(databaseId, branchId);
    } catch (err) {
      setError(errorText(err));
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
        await revoke.mutateAsync({ id: databaseId, branchId, keyId: pending.key.id });
      } else {
        // Like a created key: never through the mutation cache.
        setShown(await rotateKey(databaseId, branchId, pending.key.id));
        await refreshKeys(databaseId, branchId);
      }
      setPending(null);
    } catch (err) {
      setError(errorText(err));
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
              <th className="py-1 font-medium">Databases</th>
              <th className="py-1 font-medium">Last used</th>
              <th className="py-1 font-medium">Actions</th>
            </tr>
          </thead>
          <tbody>
            {secrets.map((k) => (
              <tr key={k.id} className="border-b last:border-0">
                <td className="py-1">
                  {k.name}
                  {k.role_blocked && (
                    <span className="mt-0.5 flex flex-col gap-0.5">
                      <span className="w-fit rounded-full border border-destructive px-2 py-0.5 text-xs text-destructive">
                        Blocked
                      </span>
                      <span className="text-xs text-muted-foreground">
                        the role {k.role} can bypass row-level security
                      </span>
                    </span>
                  )}
                </td>
                <td className="py-1 font-mono">{k.prefix}…</td>
                <td className="py-1">{k.role}</td>
                <td className="py-1">
                  <KeyDatabases databaseId={databaseId} branchId={branchId} keyId={k.id} value={k.databases} databases={databases} />
                </td>
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
        <DatabaseSelector databases={databases} value={allowed} onChange={setAllowed} />
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
