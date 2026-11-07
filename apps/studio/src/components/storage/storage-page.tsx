import { useEffect, useState, type MouseEvent, type ReactNode } from "react";
import { HeaderContext } from "@/components/app-shell/header-context";
import { ContextBar } from "@/components/table-editor/context-bar";
import { Button } from "@/components/ui/button";
import { actingCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { storageProblem, useBuckets } from "@/lib/storage-api";
import { LEVELS, readLast, resolveParams, writeLast, type EditorParams } from "@/lib/table-editor/context";
import { TableEditorStoreContext, createTableEditorStore } from "@/lib/table-editor/store";
import { usePlace } from "@/lib/table-editor/use-place";
import { useFillViewport } from "@/lib/use-fill-viewport";
import { BucketDetails } from "./buckets/bucket-details";
import { BucketDialog } from "./buckets/bucket-dialog";
import { BucketList } from "./buckets/bucket-list";

/** The Storage tab's place: down to the branch (storage runs in its main Postgres database), then a bucket and an object path. */
export interface StorageParams {
  projectId?: string;
  databaseId?: string;
  branchId?: string;
  bucket?: string;
  /** The object path inside the bucket (the route's splat). */
  path?: string;
}

/** The Storage tab's own last place (its levels stop at the branch). */
export const STORAGE_LAST_KEY = "chelabase.studio.storage.last";

export interface StoragePageProps {
  params: StorageParams;
  navigate: (p: StorageParams, opts?: { replace?: boolean }) => void;
  /** The Data API page at a place (where the data API is enabled); a plain link without it. */
  openDataApi?: (p: EditorParams) => void;
}

const PLACE_LEVELS = LEVELS.slice(0, 3);
const sameLevels = (a: StorageParams, b: StorageParams) => PLACE_LEVELS.every((l) => (a[l as keyof StorageParams] || undefined) === (b[l as keyof StorageParams] || undefined));
const placeOf = (p: StorageParams): StorageParams => Object.fromEntries(PLACE_LEVELS.flatMap((l) => (p[l as keyof StorageParams] ? [[l, p[l as keyof StorageParams]]] : [])));

/** A place's URL: each present level up to the first missing one. */
export function storagePath(p: StorageParams): string {
  let path = "/app/storage";
  for (const level of [...PLACE_LEVELS, "bucket"] as (keyof StorageParams)[]) {
    const v = p[level];
    if (!v) break;
    path += `/${encodeURIComponent(v)}`;
  }
  return path;
}

function dataApiPath(p: EditorParams): string {
  return `/app/data-api/${[p.projectId, p.databaseId, p.branchId, p.pgDatabase].filter((v): v is string => !!v).map(encodeURIComponent).join("/")}`;
}

function browserStorage(kind: "local" | "session"): Storage | null {
  try {
    return kind === "local" ? window.localStorage : window.sessionStorage;
  } catch {
    return null;
  }
}

/**
 * Studio's Storage tab: the shared context bar (project, database, branch and Act as, the table
 * editor's choice), the buckets on the left and the selected bucket on the right. Router-free: the
 * route passes the URL's params and a `navigate`.
 */
export function StoragePage(props: StoragePageProps) {
  // The table editor's store: Act as is kept per database and branch in session storage.
  const [store] = useState(() => createTableEditorStore(browserStorage("session")));
  return (
    <TableEditorStoreContext.Provider value={store}>
      <Page {...props} />
    </TableEditorStoreContext.Provider>
  );
}

function Notice({ children, alert = false }: { children: ReactNode; alert?: boolean }) {
  return (
    <p role={alert ? "alert" : "status"} className={alert ? "p-6 text-sm text-coral" : "p-6 text-sm text-chalk-dim"}>
      {children}
    </p>
  );
}

function Page({ params, navigate, openDataApi }: StoragePageProps) {
  const fill = useFillViewport();
  const [last] = useState(() => readLast(browserStorage("local"), STORAGE_LAST_KEY));
  const place = usePlace({ projectId: params.projectId, databaseId: params.databaseId, branchId: params.branchId });
  const { me, projectList, projects, databaseOk, detail, branches, branchOk, pgDatabases, pgList, roles, caps, allowed, actAs, actAsReady, actAsReason, rolesError } = place;
  const [creating, setCreating] = useState(false);

  // No Postgres database level: an empty list lets the resolver stop at the branch.
  const resolution = resolveParams(placeOf(params), last, { projects, branches: databaseOk ? branches.data : undefined, pgDatabases: [] });
  const resolved: StorageParams = resolution.kind === "redirect" || resolution.kind === "ready" ? placeOf(resolution.params) : placeOf(params);
  const redirect = resolution.kind !== "not_found" && resolution.kind !== "loading" && !sameLevels(params, resolved);
  const ready = (resolution.kind === "redirect" || resolution.kind === "ready") && !redirect;
  const current: StorageParams = { ...resolved, ...(params.bucket ? { bucket: params.bucket } : {}), ...(params.path ? { path: params.path } : {}) };
  const currentKey = PLACE_LEVELS.map((l) => resolved[l as keyof StorageParams] ?? "").join("/");
  useEffect(() => {
    if (redirect) navigate(current, { replace: true });
    // `current` is determined by `currentKey`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [redirect, currentKey]);
  useEffect(() => {
    if (ready) writeLast(browserStorage("local"), resolved, STORAGE_LAST_KEY);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, currentKey]);

  const main = pgList?.find((d) => d.main);
  const dataApiOn = main?.data_api === "on";
  // Nothing goes out until the role is known: an unknown one would go as the owner.
  const canAct = actAsReady && actAsReason === null;
  const databaseId = current.databaseId ?? "";
  const branchId = current.branchId ?? "";
  const buckets = useBuckets(databaseId, branchId, actAs, ready && branchOk && !!main && dataApiOn && canAct);
  // Changing buckets needs the role held at Developer (owner mode is an Admin's).
  const canWrite = canAct && actingCaps(caps, actAs, roles.data ?? []).atLeast("developer");
  const problem = storageProblem(buckets.error);
  const select = (bucket: string) => navigate({ ...placeOf(current), bucket });

  const dataApiPlace: EditorParams = { projectId: current.projectId, databaseId: current.databaseId, branchId: current.branchId, pgDatabase: main?.name };
  const enableLink = (event: MouseEvent<HTMLAnchorElement>) => {
    if (!openDataApi || event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) return;
    event.preventDefault();
    openDataApi(dataApiPlace);
  };
  const needsDataApi = (
    <div role="status" className="max-w-xl space-y-2 p-6 text-sm">
      <p className="font-semibold">Storage needs the data API</p>
      <p className="text-chalk-dim">
        Buckets and files live in the storage schema, which is set up in a Postgres database the data API serves
        {main ? (
          <>
            . The data API is off for <span className="font-mono">{main.name}</span>.
          </>
        ) : (
          "."
        )}
      </p>
      <a href={dataApiPath(dataApiPlace)} onClick={enableLink} className="inline-block text-sand underline-offset-4 hover:underline">
        Enable the data API
      </a>
    </div>
  );

  const placeError = projectList.error ?? me.error ?? branches.error ?? pgDatabases.error;
  let body;
  if (resolution.kind === "not_found") {
    body = <Notice>{{ project: "Project", database: "Database", branch: "Branch", pgDatabase: "Postgres database", schema: "Schema", table: "Table" }[resolution.level]} not found</Notice>;
  } else if (placeError) {
    body = <Notice alert>{errorText(placeError)}</Notice>;
  } else if (!ready) {
    body = <Notice>Loading…</Notice>;
  } else if (!current.projectId) {
    body = <Notice>No projects yet</Notice>;
  } else if (!current.databaseId) {
    body = <Notice>This project has no databases</Notice>;
  } else if (!current.branchId) {
    body = <Notice>No ready branch</Notice>;
  } else if (!pgList) {
    body = <Notice>Loading…</Notice>;
  } else if (!main) {
    body = <Notice>This branch has no main Postgres database</Notice>;
  } else if (!dataApiOn || problem === "data_api") {
    body = needsDataApi;
  } else if (!canAct) {
    body = <Notice>{actAsReason ?? "Loading roles…"}</Notice>;
  } else if (problem === "unavailable") {
    body = <Notice>Storage isn&apos;t ready in this database yet. Try again in a moment.</Notice>;
  } else if (buckets.error) {
    body = <Notice alert>{errorText(buckets.error)}</Notice>;
  } else if (!buckets.data) {
    body = <Notice>Loading buckets…</Notice>;
  } else if (buckets.data.length === 0) {
    body = (
      <div className="max-w-xl space-y-3 p-6 text-sm">
        <p className="font-semibold">No buckets on this branch</p>
        <p className="text-chalk-dim">
          A bucket holds files under one set of rules: public or private, a size limit and the file types it accepts. Row level security on
          storage decides who reads and writes each file.
        </p>
        {canWrite && <Button onClick={() => setCreating(true)}>Create a bucket</Button>}
      </div>
    );
  } else {
    const selected = current.bucket ? buckets.data.find((b) => b.name === current.bucket) : undefined;
    body = (
      <div className="flex min-h-0 flex-1">
        <aside className="flex w-72 shrink-0 flex-col border-r border-line">
          <div className="flex h-11 shrink-0 items-center justify-between gap-2 border-b border-line px-4">
            <h2 className="text-sm font-semibold">Buckets</h2>
            {canWrite && (
              <Button size="sm" variant="outline" onClick={() => setCreating(true)}>
                New bucket
              </Button>
            )}
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto">
            <BucketList
              databaseId={databaseId}
              branchId={branchId}
              role={actAs}
              buckets={buckets.data}
              selected={current.bucket}
              hrefFor={(name) => storagePath({ ...placeOf(current), bucket: name })}
              onSelect={select}
            />
          </div>
        </aside>
        <div className="min-w-0 flex-1 overflow-y-auto">
          {selected ? (
            <BucketDetails
              key={selected.name}
              databaseId={databaseId}
              branchId={branchId}
              role={actAs}
              bucket={selected}
              canWrite={canWrite}
              onDeleted={() => navigate(placeOf(current))}
            />
          ) : current.bucket ? (
            <Notice>Bucket not found</Notice>
          ) : (
            <Notice>Pick a bucket to see its settings.</Notice>
          )}
        </div>
      </div>
    );
  }

  return (
    <div ref={fill.ref} style={fill.style} className="flex min-h-0 flex-col">
      <HeaderContext>
        <ContextBar
          params={current}
          navigate={(p) => navigate(placeOf(p))}
          projects={projects}
          branches={databaseOk ? branches.data : undefined}
          pgLevel={false}
          actAs={actAsReady ? { owner: allowed.owner, roles: allowed.roles, selected: actAs } : undefined}
          onActAs={place.setActAs}
          rolesError={rolesError}
          historyWindow={detail.data?.history_window}
          onBranchesOpen={() => void branches.refetch()}
          onBranchCreated={(b) => place.setCreatedBranchId(b.id)}
        />
      </HeaderContext>
      <div className="flex min-h-0 flex-1 flex-col">{body}</div>
      {canWrite && current.databaseId && current.branchId && (
        <BucketDialog open={creating} onOpenChange={setCreating} databaseId={databaseId} branchId={branchId} role={actAs} onCreated={select} />
      )}
    </div>
  );
}
