import { useCallback, useEffect, useId, useState } from "react";
import type { BranchSummary, DataApiSchema, DataKey } from "@cheladb/types";
import { Button } from "@/components/ui/button";
import type { ApiClient } from "@/lib/api";
import { CopyButton } from "./copy-button";
import { DataApiKeys } from "./data-api-keys";
import { RlsWarning } from "./rls-warning";

export interface DataApiPanelProps {
  api: ApiClient;
  databaseId: string;
  branch: BranchSummary;
  /** The gateway's base URL (`VITE_API_URL`); the endpoint is `${apiUrl}/data/v1/query`. */
  apiUrl: string;
}

const message = (err: unknown) => (err instanceof Error ? err.message : String(err));

/** One branch's data API: endpoint, public key, secret keys and the RLS warning. */
export function DataApiPanel({ api, databaseId, branch, apiUrl }: DataApiPanelProps) {
  const [keys, setKeys] = useState<DataKey[] | null>(null);
  const [schema, setSchema] = useState<DataApiSchema | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [schemaError, setSchemaError] = useState<string | null>(null);
  const [rotating, setRotating] = useState(false);
  const [busy, setBusy] = useState(false);
  const branchId = branch.id as string;
  const endpoint = `${apiUrl.replace(/\/+$/, "")}/data/v1/query`;

  const loadKeys = useCallback(async () => {
    try {
      setKeys(await api.listKeys(databaseId, branchId));
      setError(null);
    } catch (err) {
      setError(message(err));
    }
  }, [api, databaseId, branchId]);

  useEffect(() => {
    void loadKeys();
    api
      .getDataApiSchema(databaseId, branchId)
      .then((s) => {
        setSchema(s);
        setSchemaError(null);
      })
      .catch((err) => setSchemaError(`Couldn't check RLS: ${message(err)}`));
  }, [api, databaseId, branchId, loadKeys]);

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
    try {
      await api.rotateKey(databaseId, branchId, publicKey.id);
      setRotating(false);
      await loadKeys();
    } catch (err) {
      setError(message(err));
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
        <DataApiKeys api={api} databaseId={databaseId} branchId={branchId} keys={keys} onChanged={() => void loadKeys()} />
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
