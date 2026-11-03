import { useEffect, useRef, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  getGetAppUsersQueryKey,
  getGetProjectQueryKey,
  useDisableAppUsers,
  useGetAppUsers,
  useListBranchDatabases,
  useListBranches,
  usePutAppUsers,
  type AppUsersSettings,
  type DatabaseSummary,
  type ProjectInfo,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { appUserErrorText } from "@/lib/app-user-errors";
import { POLL_MS, pollWhile } from "@/lib/query";

export interface AppUsersCardProps {
  project: ProjectInfo;
  /** undefined until the workspace has loaded. */
  databases: DatabaseSummary[] | undefined;
}

const input = "h-8 rounded-lg border border-input bg-background px-2 text-sm";
const NAME_RE = /^[a-z_][a-z0-9_]{0,62}$/;

export function AppUsersCard({ project, databases }: AppUsersCardProps) {
  const queryClient = useQueryClient();
  // After enabling from this card the project query may lag behind; the settings are known.
  const [started, setStarted] = useState(false);
  // A finished disable answers 404 app_users_disabled; stays set until the project catches up.
  const [disabled, setDisabled] = useState(false);
  const known = (project.app_users !== null || started) && !disabled;
  const settings = useGetAppUsers(project.id, {
    query: {
      enabled: known,
      refetchInterval: pollWhile<AppUsersSettings>((s) => s.status === "enabling" || s.status === "disabling"),
    },
  });
  const gone = settings.error instanceof ApiError && settings.error.code === "app_users_disabled";
  const status = known ? (settings.data?.status ?? project.app_users) : null;
  const refreshProject = () => queryClient.invalidateQueries({ queryKey: getGetProjectQueryKey(project.id) });

  useEffect(() => {
    if (gone) {
      setDisabled(true);
      setStarted(false);
      void refreshProject();
    }
  }, [gone]);
  useEffect(() => {
    if (project.app_users === null) setDisabled(false);
  }, [project.app_users]);

  // Keep the project (and so the Users tab) in step with the settings.
  const seen = settings.data?.status;
  useEffect(() => {
    if (seen && seen !== project.app_users) void refreshProject();
  }, [seen, project.app_users]);
  const transitional = status === "enabling" || status === "disabling";
  useEffect(() => {
    if (!transitional) return;
    const t = setInterval(() => void refreshProject(), POLL_MS);
    return () => clearInterval(t);
  }, [transitional]);

  const put = usePutAppUsers({
    mutation: {
      onSuccess: (data) => {
        setStarted(true);
        setDisabled(false);
        queryClient.setQueryData(getGetAppUsersQueryKey(project.id), data);
        void queryClient.invalidateQueries({ queryKey: getGetProjectQueryKey(project.id) });
      },
    },
  });
  const disable = useDisableAppUsers({
    mutation: {
      onSuccess: (data) => {
        queryClient.setQueryData(getGetAppUsersQueryKey(project.id), data);
        void queryClient.invalidateQueries({ queryKey: getGetProjectQueryKey(project.id) });
      },
    },
  });

  return (
    <section data-slot="app-users-card" className="space-y-3">
      <h2 className="text-lg font-semibold">App users</h2>
      {status === null ? (
        <EnableForm
          databases={databases}
          pending={put.isPending}
          onSubmit={(data) => put.mutate({ id: project.id, data })}
        />
      ) : status === "enabling" || status === "disabling" ? (
        <p className="text-sm text-muted-foreground">{status === "enabling" ? "Enabling…" : "Disabling…"}</p>
      ) : settings.data === undefined ? (
        <p className="text-sm text-muted-foreground">{settings.error ? appUserErrorText(settings.error) : "Loading…"}</p>
      ) : status === "failed" ? (
        <div className="space-y-2 text-sm">
          <p role="status" className="text-destructive">
            Enabling app users failed
          </p>
          {settings.data.error && <p>{settings.data.error}</p>}
          <div className="flex gap-2">
            <Button
              disabled={put.isPending || disable.isPending}
              onClick={() =>
                put.mutate({
                  id: project.id,
                  data: {
                    database_id: settings.data.database_id,
                    pg_database: settings.data.pg_database,
                    schema: settings.data.schema,
                    table: settings.data.table,
                    email_verification: settings.data.email_verification,
                    on_delete: settings.data.on_delete,
                  },
                })
              }
            >
              Retry
            </Button>
            <Button variant="outline" disabled={put.isPending || disable.isPending} onClick={() => disable.mutate({ id: project.id })}>
              Disable
            </Button>
          </div>
        </div>
      ) : (
        <EnabledSettings
          settings={settings.data}
          databases={databases}
          saving={put.isPending}
          disabling={disable.isPending}
          onSave={(data) => put.mutate({ id: project.id, data })}
          onDisable={() => disable.mutate({ id: project.id })}
        />
      )}
      {(put.error || disable.error) && (
        <p role="alert" className="text-sm text-destructive">
          {appUserErrorText(put.error ?? disable.error)}
        </p>
      )}
    </section>
  );
}

/** The ready database's Postgres databases that serve the data API (main branch). */
function usePgDatabases(databaseId: string | undefined) {
  const branches = useListBranches(databaseId ?? "", { query: { enabled: !!databaseId } });
  const main = branches.data?.find((b) => b.parent_branch_id === null);
  const list = useListBranchDatabases(databaseId ?? "", main?.id ?? "", { query: { enabled: !!databaseId && !!main } });
  const served = (list.data ?? []).filter((d) => d.origin !== "system" && d.data_api === "on");
  return { served, loading: branches.isPending || (!!main && list.isPending), error: branches.error ?? list.error };
}

function Verification({ checked, onChange }: { checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className="flex items-center gap-2 text-sm">
      <input type="checkbox" role="switch" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      <span>Require email verification</span>
    </label>
  );
}

function OnDeleteSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  return (
    <label className="flex flex-col gap-1 text-sm">
      <span>When a user is deleted in the identity service</span>
      <select value={value} onChange={(e) => onChange(e.target.value)} className={input}>
        <option value="delete">Delete the row</option>
        <option value="mark">Mark the row (keep it)</option>
      </select>
    </label>
  );
}

function EnableForm({
  databases,
  pending,
  onSubmit,
}: {
  databases: DatabaseSummary[] | undefined;
  pending: boolean;
  onSubmit: (data: { database_id: string; pg_database: string; schema: string; table: string; email_verification: boolean; on_delete: string }) => void;
}) {
  const ready = (databases ?? []).filter((d) => d.status === "ready");
  const [databaseId, setDatabaseId] = useState("");
  const [pgDatabase, setPgDatabase] = useState("");
  const [schema, setSchema] = useState("public");
  const [table, setTable] = useState("users");
  const [verification, setVerification] = useState(false);
  const [onDelete, setOnDelete] = useState("delete");
  const chosenDb = ready.find((d) => d.database_id === databaseId)?.database_id ?? ready[0]?.database_id;
  const pg = usePgDatabases(chosenDb);
  const chosenPg = pg.served.find((d) => d.name === pgDatabase)?.name ?? (pg.served.find((d) => d.main) ?? pg.served[0])?.name;
  const valid = NAME_RE.test(schema) && NAME_RE.test(table);

  function submit(event: FormEvent) {
    event.preventDefault();
    if (!chosenDb || !chosenPg || !valid || pending) return;
    onSubmit({ database_id: chosenDb, pg_database: chosenPg, schema, table, email_verification: verification, on_delete: onDelete });
  }

  return (
    <form onSubmit={submit} className="space-y-3">
      <p className="text-sm text-muted-foreground">
        Give this project sign-in for its own users. Chelabase creates the users table and keeps it in step with the
        project's identity provider. RLS is on with own-row policies by default. Sign-up data needs a grant per column:{" "}
        <code>GRANT INSERT (col) ON &lt;table&gt; TO authenticated</code>.
      </p>
      <div className="flex flex-wrap items-end gap-3">
        <label className="flex flex-col gap-1 text-sm">
          <span>Database</span>
          <select value={chosenDb ?? ""} onChange={(e) => { setDatabaseId(e.target.value); setPgDatabase(""); }} className={input}>
            {ready.map((d) => (
              <option key={d.database_id} value={d.database_id}>
                {d.name}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span>Postgres database</span>
          <select value={chosenPg ?? ""} onChange={(e) => setPgDatabase(e.target.value)} className={input}>
            {pg.served.map((d) => (
              <option key={d.name} value={d.name}>
                {d.name}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span>Schema</span>
          <input value={schema} onChange={(e) => setSchema(e.target.value)} className={`${input} w-40`} autoComplete="off" />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span>Table</span>
          <input value={table} onChange={(e) => setTable(e.target.value)} className={`${input} w-40`} autoComplete="off" />
        </label>
      </div>
      {!valid && <p className="text-sm text-destructive">Schema and table: 1-63 characters from a-z, 0-9 and _, not starting with a digit.</p>}
      {chosenDb && !pg.loading && pg.served.length === 0 && (
        <p className="text-sm text-muted-foreground">Enable the data API on a Postgres database of this database first.</p>
      )}
      {ready.length === 0 && databases !== undefined && (
        <p className="text-sm text-muted-foreground">Create a database first.</p>
      )}
      <Verification checked={verification} onChange={setVerification} />
      <OnDeleteSelect value={onDelete} onChange={setOnDelete} />
      <Button type="submit" disabled={!chosenDb || !chosenPg || !valid || pending}>
        Enable app users
      </Button>
    </form>
  );
}

function EnabledSettings({
  settings,
  databases,
  saving,
  disabling,
  onSave,
  onDisable,
}: {
  settings: AppUsersSettings;
  databases: DatabaseSummary[] | undefined;
  saving: boolean;
  disabling: boolean;
  onSave: (data: { database_id: string; pg_database: string; schema: string; table: string; email_verification: boolean; on_delete: string }) => void;
  onDisable: () => void;
}) {
  const [verification, setVerification] = useState(settings.email_verification);
  const [onDelete, setOnDelete] = useState<string>(settings.on_delete);
  useEffect(() => {
    setVerification(settings.email_verification);
    setOnDelete(settings.on_delete);
  }, [settings.email_verification, settings.on_delete]);
  // A deleting project's workspace may stop listing its databases: keep the name once seen, the uuid only as a last resort.
  const seenName = useRef<{ id: string; name: string } | null>(null);
  const found = databases?.find((d) => d.database_id === settings.database_id)?.name;
  if (found) seenName.current = { id: settings.database_id, name: found };
  const dbName = found ?? (seenName.current?.id === settings.database_id ? seenName.current.name : settings.database_id);
  const dirty = verification !== settings.email_verification || onDelete !== settings.on_delete;
  return (
    <div className="space-y-3">
      <p className="text-sm">
        Users live in <code>{dbName}</code> / <code>{settings.pg_database}</code>, table{" "}
        <code>
          {settings.schema}.{settings.table}
        </code>
        .
      </p>
      <Verification checked={verification} onChange={setVerification} />
      <OnDeleteSelect value={onDelete} onChange={setOnDelete} />
      <div className="flex gap-2">
        <Button
          disabled={!dirty || saving}
          onClick={() =>
            onSave({
              database_id: settings.database_id,
              pg_database: settings.pg_database,
              schema: settings.schema,
              table: settings.table,
              email_verification: verification,
              on_delete: onDelete,
            })
          }
        >
          Save settings
        </Button>
        <Button variant="outline" disabled={disabling} onClick={onDisable}>
          Disable app users
        </Button>
      </div>
    </div>
  );
}
