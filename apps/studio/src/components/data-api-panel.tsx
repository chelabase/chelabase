import { useId, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  getListKeysQueryKey,
  rotateKey,
  useGetDataApiSchema,
  useListBranchDatabases,
  useListKeys,
  type Branch,
  type BranchDatabaseView,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { useProjectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { pollWhile } from "@/lib/query";
import { CopyButton } from "./copy-button";
import { DataApiDatabases, KeyDatabases } from "./data-api-databases";
import { DataApiKeys } from "./data-api-keys";
import { DataApiTypes } from "./data-api-types";
import { RlsWarning } from "./rls-warning";

export interface DataApiPanelProps {
  databaseId: string;
  branch: Branch;
  /** The gateway's base URL (`VITE_API_URL`); the endpoint is `${apiUrl}/data/v1/query`. */
  apiUrl: string;
  /** The Postgres database picked (the Connect panel's header picks it); left out or unknown, the branch's main one. */
  pgDatabase?: string;
}

/**
 * One branch's data API: the keys, the branch's Postgres databases (with the one picked) and the
 * served database's schema, and rotating the public key. The sections below share one of these,
 * so the Connect panel and the Data API page send the same requests.
 */
export function useDataApi(databaseId: string, branchId: string, pgDatabase: string | undefined) {
  const queryClient = useQueryClient();
  const keyList = useListKeys(databaseId, branchId);
  // While a database is enabling, keep the list fresh until it turns on or fails.
  const dbList = useListBranchDatabases(databaseId, branchId, {
    query: { refetchInterval: pollWhile<BranchDatabaseView[]>((ds) => ds.some((d) => d.data_api === "enabling")) },
  });
  const databases = dbList.data ?? null;
  const pickedDb = databases?.find((d) => d.name === pgDatabase) ?? databases?.find((d) => d.main) ?? databases?.[0];
  // The schema (and the RLS warning) are read from a served database only.
  const schemaQuery = useGetDataApiSchema(
    databaseId,
    branchId,
    { database: pickedDb?.name },
    { query: { enabled: pickedDb?.data_api === "on" } },
  );
  const [rotating, setRotating] = useState(false);
  const [busy, setBusy] = useState(false);
  const [rotateError, setRotateError] = useState<string | null>(null);
  const keys = keyList.data ?? null;
  const schema = pickedDb?.data_api === "on" ? (schemaQuery.data ?? null) : null;
  const error = keyList.error ? errorText(keyList.error) : dbList.error ? errorText(dbList.error) : rotateError;
  const schemaError = pickedDb?.data_api === "on" && schemaQuery.error ? `Couldn't check RLS: ${errorText(schemaQuery.error)}` : null;
  const publicKey = keys?.find((k) => k.kind === "public");

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

  return { databaseId, branchId, keys, databases, pickedDb, schema, error, schemaError, publicKey, rotating, setRotating, busy, rotatePublic };
}

export type DataApi = ReturnType<typeof useDataApi>;

/** The endpoint and the public key, each with Copy, and Rotate public key (Developer's, rotateKey). */
export function DataApiEndpoint({ api, apiUrl }: { api: DataApi; apiUrl: string }) {
  const canRotate = useProjectCaps().atLeast("developer");
  const { keys, publicKey, busy, rotating, setRotating, rotatePublic } = api;
  const endpoint = `${apiUrl.replace(/\/+$/, "")}/data/v1/query`;
  const reasonId = useId();
  const rotateReason = busy
    ? "Working on a key change."
    : keys === null
      ? "Waiting for the keys to load."
      : !publicKey
        ? "This branch has no public key."
        : null;

  return (
    <>
      <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-4 gap-y-2">
        <dt className="text-muted-foreground">Endpoint</dt>
        <dd className="min-w-0 space-y-1">
          <div className="font-mono break-all">{endpoint}</div>
          <div className="flex flex-wrap items-center gap-2">
            <CopyButton value={endpoint} label="Copy endpoint" />
          </div>
        </dd>
        <dt className="text-muted-foreground">Public key</dt>
        <dd className="min-w-0 space-y-1">
          <div className="font-mono break-all">{publicKey ? (publicKey.key ?? `${publicKey.prefix}…`) : "—"}</div>
          <div className="flex flex-wrap items-center gap-2">
            {publicKey?.key && <CopyButton value={publicKey.key} label="Copy public key" />}
            {canRotate && (
              <Button
                size="xs"
                variant="outline"
                disabled={rotateReason !== null}
                aria-describedby={rotateReason ? reasonId : undefined}
                onClick={() => setRotating(true)}
              >
                Rotate public key
              </Button>
            )}
          </div>
        </dd>
      </dl>
      {rotateReason && (
        <p id={reasonId} className="text-xs text-muted-foreground">
          {rotateReason}
        </p>
      )}
      {rotating && (
        <div className="space-y-2 border p-3">
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
    </>
  );
}

/** Whether the data API serves the picked Postgres database, and which databases the public key reaches. */
export function DataApiExposure({ api }: { api: DataApi }) {
  const { databaseId, branchId, pickedDb, publicKey, databases } = api;
  return (
    <>
      {pickedDb && <DataApiDatabases databaseId={databaseId} branchId={branchId} database={pickedDb} />}
      {publicKey && databases && (
        <section aria-label="Public key databases" className="space-y-1">
          <h4 className="text-muted-foreground">Public key databases</h4>
          <KeyDatabases
            key={`${publicKey.id}/${(publicKey.databases ?? ["*"]).join(",")}`}
            databaseId={databaseId}
            branchId={branchId}
            keyId={publicKey.id}
            value={publicKey.databases}
            databases={databases}
          />
        </section>
      )}
    </>
  );
}

/** The RLS warning for the served database's exposed tables (nothing while it has none). */
export function DataApiRls({ api }: { api: DataApi }) {
  return (
    <>
      {api.schemaError && (
        <p role="alert" className="text-destructive">
          {api.schemaError}
        </p>
      )}
      {api.schema && <RlsWarning schema={api.schema} />}
    </>
  );
}

/** The secret keys (list, create, revoke), with the keys' or databases' load error. */
export function DataApiSecretKeys({ api }: { api: DataApi }) {
  const { databaseId, branchId, keys, databases, error } = api;
  return (
    <>
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
      {keys === null && !error && <p className="text-muted-foreground">Loading keys…</p>}
      {keys !== null && <DataApiKeys databaseId={databaseId} branchId={branchId} keys={keys} databases={databases ?? []} />}
    </>
  );
}

/** Downloading types is creating a key's access (Developer, by role). */
export function DataApiTypesFor({ api }: { api: DataApi }) {
  const canDownload = useProjectCaps().atLeast("developer");
  const { databaseId, branchId, databases, pickedDb } = api;
  if (!canDownload || databases === null) return null;
  return <DataApiTypes databaseId={databaseId} branchId={branchId} databases={databases} defaultDatabase={pickedDb?.name} />;
}

/**
 * One branch's data API in two columns that stack on narrow screens: the endpoint, public key and
 * what it reaches, then the secret keys and client setup.
 */
export function DataApiPanel({ databaseId, branch, apiUrl, pgDatabase }: DataApiPanelProps) {
  const api = useDataApi(databaseId, branch.id, pgDatabase);
  return (
    <div
      aria-label={`Data API for ${branch.name}`}
      role="group"
      className="grid gap-4 text-sm [grid-template-columns:repeat(auto-fit,minmax(320px,1fr))]"
    >
      <section aria-label="Data API" className="min-w-0 space-y-3 border p-3">
        <h3 className="font-medium">Data API</h3>
        <DataApiEndpoint api={api} apiUrl={apiUrl} />
        <DataApiExposure api={api} />
        <DataApiRls api={api} />
      </section>
      <section aria-label="Secret keys and client setup" className="min-w-0 space-y-3 border p-3">
        <DataApiSecretKeys api={api} />
        <DataApiTypesFor api={api} />
      </section>
    </div>
  );
}
