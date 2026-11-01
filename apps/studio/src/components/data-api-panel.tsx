import { useId, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
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
import { DataApiDatabases, DatabasePicker, KeyDatabases } from "./data-api-databases";
import { DataApiKeys } from "./data-api-keys";
import { DataApiTypes } from "./data-api-types";
import { RlsWarning } from "./rls-warning";

export interface DataApiPanelProps {
  databaseId: string;
  branch: Branch;
  /** The gateway's base URL (`VITE_API_URL`); the endpoint is `${apiUrl}/data/v1/query`. */
  apiUrl: string;
  /** Extra content at the end of the second column (the Connect panel's logins line). */
  aside?: ReactNode;
  /** Where to render the Postgres DB picker (the Connect panel's header); without it the picker sits in the first column. */
  pickerSlot?: HTMLElement | null;
}

/**
 * One branch's data API in two columns that stack on narrow screens: the endpoint, public key and
 * what it reaches, then the secret keys and client setup.
 */
export function DataApiPanel({ databaseId, branch, apiUrl, aside, pickerSlot }: DataApiPanelProps) {
  // Rotating the public key is Developer's (rotateKey).
  const canRotate = useProjectCaps().atLeast("developer");
  const queryClient = useQueryClient();
  const branchId = branch.id;
  const keyList = useListKeys(databaseId, branchId);
  // While a database is enabling, keep the list fresh until it turns on or fails.
  const dbList = useListBranchDatabases(databaseId, branchId, {
    query: { refetchInterval: pollWhile<BranchDatabaseView[]>((ds) => ds.some((d) => d.data_api === "enabling")) },
  });
  const databases = dbList.data ?? null;
  const [pickedName, setPickedName] = useState<string | null>(null);
  const pickedDb = databases?.find((d) => d.name === pickedName) ?? databases?.find((d) => d.main) ?? databases?.[0];
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
    <div
      aria-label={`Data API for ${branch.name}`}
      role="group"
      className="grid gap-4 text-sm [grid-template-columns:repeat(auto-fit,minmax(320px,1fr))]"
    >
      <section aria-label="Data API" className="min-w-0 space-y-3 border p-3">
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
        {pickerSlot && databases !== null && pickedDb && createPortal(
          <DatabasePicker label="Postgres DB" databases={databases} value={pickedDb.name} onPick={setPickedName} />,
          pickerSlot,
        )}
        {databases !== null && databases.length > 0 && pickedDb && (
          <DataApiDatabases
            hidePicker={!!pickerSlot}
            databaseId={databaseId}
            branchId={branchId}
            databases={databases}
            picked={pickedDb.name}
            onPick={setPickedName}
          />
        )}
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
        {schemaError && (
          <p role="alert" className="text-destructive">
            {schemaError}
          </p>
        )}
        {schema && <RlsWarning schema={schema} />}
      </section>
      <section aria-label="Secret keys and client setup" className="min-w-0 space-y-3 border p-3">
        {error && (
          <p role="alert" className="text-destructive">
            {error}
          </p>
        )}
        {keys === null && !error && <p className="text-muted-foreground">Loading keys…</p>}
        {keys !== null && (
          <DataApiKeys databaseId={databaseId} branchId={branchId} keys={keys} databases={databases ?? []} />
        )}
        {/* Downloading types is creating a key's access (Developer, by role). */}
        {canRotate && databases !== null && (
          <DataApiTypes databaseId={databaseId} branchId={branchId} databases={databases} defaultDatabase={pickedDb?.name} />
        )}
        {aside}
      </section>
    </div>
  );
}
