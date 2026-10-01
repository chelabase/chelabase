import { useId, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { getListKeysQueryKey, rotateKey, useGetDataApiSchema, useListKeys, type Branch } from "@cheladb/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import { CopyButton } from "./copy-button";
import { DataApiKeys } from "./data-api-keys";
import { RlsWarning } from "./rls-warning";

export interface DataApiPanelProps {
  databaseId: string;
  branch: Branch;
  /** The gateway's base URL (`VITE_API_URL`); the endpoint is `${apiUrl}/data/v1/query`. */
  apiUrl: string;
}

/** One branch's data API: endpoint, public key, secret keys and the RLS warning. */
export function DataApiPanel({ databaseId, branch, apiUrl }: DataApiPanelProps) {
  const queryClient = useQueryClient();
  const branchId = branch.id;
  const keyList = useListKeys(databaseId, branchId);
  const schemaQuery = useGetDataApiSchema(databaseId, branchId);
  const [rotating, setRotating] = useState(false);
  const [busy, setBusy] = useState(false);
  const [rotateError, setRotateError] = useState<string | null>(null);
  const keys = keyList.data ?? null;
  const schema = schemaQuery.data ?? null;
  const error = keyList.error ? errorText(keyList.error) : rotateError;
  const schemaError = schemaQuery.error ? `Couldn't check RLS: ${errorText(schemaQuery.error)}` : null;
  const endpoint = `${apiUrl.replace(/\/+$/, "")}/data/v1/query`;

  const publicKey = keys?.find((k) => k.kind === "public");
  const reasonId = useId();
  const rotateReason = busy
    ? "Working on a key change."
    : keys === null
      ? "Waiting for the keys to load."
      : !publicKey
        ? "This branch has no public key."
        : null;

  async function rotatePublic() {
    if (!publicKey) return;
    setBusy(true);
    setRotateError(null);
    try {
      // The plain function, so the new key never sits in the mutation cache.
      await rotateKey(databaseId, branchId, publicKey.id);
      setRotating(false);
      await queryClient.invalidateQueries({ queryKey: getListKeysQueryKey(databaseId, branchId) });
    } catch (err) {
      setRotateError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section aria-label={`Data API for ${branch.name}`} className="space-y-3 rounded-lg border p-3 text-sm">
      <h3 className="font-medium">Data API</h3>
      <dl className="grid grid-cols-[auto_1fr_auto] items-center gap-x-4 gap-y-1">
        <dt className="text-muted-foreground">Endpoint</dt>
        <dd className="font-mono break-all">{endpoint}</dd>
        <dd>
          <CopyButton value={endpoint} label="Copy endpoint" />
        </dd>
        <dt className="text-muted-foreground">Public key</dt>
        <dd className="font-mono break-all">{publicKey ? (publicKey.key ?? `${publicKey.prefix}…`) : "—"}</dd>
        <dd className="flex items-center gap-2">
          {publicKey?.key && <CopyButton value={publicKey.key} label="Copy public key" />}
          <Button
            size="xs"
            variant="outline"
            disabled={rotateReason !== null}
            aria-describedby={rotateReason ? reasonId : undefined}
            onClick={() => setRotating(true)}
          >
            Rotate public key
          </Button>
        </dd>
      </dl>
      {rotateReason && (
        <p id={reasonId} className="text-xs text-muted-foreground">
          {rotateReason}
        </p>
      )}
      {rotating && (
        <div className="space-y-2 rounded-lg border p-3">
          <p>Rotate the public key? The current key stops working within 30 seconds.</p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={() => void rotatePublic()}>
              Confirm rotate public key
            </Button>
            <Button size="sm" variant="outline" onClick={() => setRotating(false)}>
              Cancel
            </Button>
          </div>
        </div>
      )}
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
      {keys === null && !error && <p className="text-muted-foreground">Loading keys…</p>}
      {keys !== null && (
        <DataApiKeys databaseId={databaseId} branchId={branchId} keys={keys} />
      )}
      {schemaError && (
        <p role="alert" className="text-destructive">
          {schemaError}
        </p>
      )}
      {schema && <RlsWarning schema={schema} />}
    </section>
  );
}
