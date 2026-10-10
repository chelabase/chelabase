import { useId, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  disableDataApi,
  enableDataApi,
  getListBranchDatabasesQueryKey,
  getListKeysQueryKey,
  updateKey,
  updatePgDatabase,
  type BranchDatabaseView,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { useProjectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";

/** The names a key's allow-list may hold: the branch's databases except the system one. */
const selectable = (databases: BranchDatabaseView[]) => databases.filter((d) => d.origin !== "system");
const MAX_ALLOWED = 20;
const SCHEMA_RE = /^[a-z_][a-z0-9_]{0,62}$/;
const SCHEMA_MESSAGE = "Default schema must be 1-63 characters from a-z, 0-9 and _, not starting with a digit.";

export function stateLabel(d: BranchDatabaseView): string {
  if (d.origin === "system") return "Not served";
  switch (d.data_api) {
    case "on":
      return "Served";
    case "enabling":
      return "Enabling…";
    case "error":
      return "Error";
    default:
      return "Not served";
  }
}

const optionText = (d: BranchDatabaseView) =>
  `${d.name}${d.origin === "system" ? " (System)" : d.main ? " (main)" : ""} · ${stateLabel(d)}`;

export interface DataApiDatabasesProps {
  databaseId: string;
  branchId: string;
  databases: BranchDatabaseView[];
  picked: string;
  onPick: (name: string) => void;
}

/** The branch's databases: a picker with each state, and the picked one's controls. */
export function DataApiDatabases({ databaseId, branchId, databases, picked, onPick }: DataApiDatabasesProps) {
  const current = databases.find((d) => d.name === picked) ?? databases[0];
  return (
    <div className="space-y-2">
      <label className="flex items-center gap-2">
        <span>Database</span>
        <select
          value={current?.name ?? ""}
          onChange={(e) => onPick(e.target.value)}
          className="h-8 border border-input bg-background px-2 text-sm"
        >
          {databases.map((d) => (
            <option key={d.name} value={d.name}>
              {optionText(d)}
            </option>
          ))}
        </select>
      </label>
      {current && (
        <DatabaseControls
          // A fresh form per database, and per saved schema.
          key={`${current.name}/${current.default_schema}`}
          databaseId={databaseId}
          branchId={branchId}
          database={current}
        />
      )}
    </div>
  );
}

type Confirm = "disable" | "remove" | null;

function DatabaseControls({
  databaseId,
  branchId,
  database,
}: {
  databaseId: string;
  branchId: string;
  database: BranchDatabaseView;
}) {
  const queryClient = useQueryClient();
  // Enabling, disabling and changing a Postgres database are Admin's; others see the state.
  const admin = useProjectCaps().atLeast("admin");
  const [confirm, setConfirm] = useState<Confirm>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [schema, setSchema] = useState(database.default_schema);
  const [grant, setGrant] = useState(false);
  const reasonId = useId();
  const helpId = useId();
  const refresh = () =>
    queryClient.invalidateQueries({ queryKey: getListBranchDatabasesQueryKey(databaseId, branchId) });
  const { name } = database;
  const system = database.origin === "system";
  const served = database.data_api === "on";

  async function run(action: () => Promise<unknown>): Promise<boolean> {
    setBusy(true);
    setError(null);
    try {
      await action();
      setConfirm(null);
      await refresh();
      return true;
    } catch (err) {
      setError(errorText(err));
      return false;
    } finally {
      setBusy(false);
    }
  }

  const schemaValid = SCHEMA_RE.test(schema);
  const changed = schema !== database.default_schema;
  const saveReason = busy
    ? "Saving the default schema…"
    : !schemaValid
      ? SCHEMA_MESSAGE
      : !changed && !grant
        ? "Change the schema, or tick the box to grant again."
        : null;
  function saveSchema() {
    if (saveReason) return;
    void run(() =>
      updatePgDatabase(databaseId, branchId, name, {
        default_schema: schema,
        ...(grant && served ? { grant_existing: true } : {}),
      }),
    ).then((ok) => ok && setGrant(false));
  }

  return (
    <div className="space-y-2 border p-3">
      <p>
        <span className="font-medium">{name}</span> · {stateLabel(database)}
      </p>
      {database.data_api === "error" && database.data_api_error && (
        <p className="text-destructive">{database.data_api_error}</p>
      )}
      {system && <p className="text-muted-foreground">The system database can&apos;t be served by the data API.</p>}
      {!system && database.main && (
        <p className="text-muted-foreground">The main database always stays served.</p>
      )}
      {!system && database.data_api === "enabling" && (
        <p className="text-muted-foreground">Enabling the data API…</p>
      )}
      {!admin && !system && <p className="text-muted-foreground">Only project Admins change the data API.</p>}
      {admin && (
      <div className="flex flex-wrap gap-2">
        {!system && (database.data_api === "off" || database.data_api === "error") && (
          <Button size="sm" disabled={busy} onClick={() => void run(() => enableDataApi(databaseId, branchId, name))}>
            Enable data API
          </Button>
        )}
        {!system && !database.main && database.data_api === "on" && (
          <>
            <Button size="sm" variant="outline" disabled={busy} onClick={() => setConfirm("disable")}>
              Disable data API
            </Button>
            <Button size="sm" variant="outline" disabled={busy} onClick={() => setConfirm("remove")}>
              Remove data API machinery
            </Button>
          </>
        )}
      </div>
      )}
      {admin && confirm && (
        <div className="space-y-2 border p-3">
          <p>
            {confirm === "disable"
              ? `Disable the data API for ${name}? Requests for it stop at once. Its grants and the chelabase schema stay.`
              : `Remove the data API machinery from ${name}? This disables it and drops the chelabase schema and its event trigger. Grants made to anon and authenticated stay.`}
          </p>
          <div className="flex gap-2">
            <Button
              size="sm"
              disabled={busy}
              onClick={() =>
                void run(() =>
                  disableDataApi(databaseId, branchId, name, confirm === "remove" ? { remove: true } : undefined),
                )
              }
            >
              {confirm === "disable" ? "Confirm disable" : "Confirm remove"}
            </Button>
            <Button size="sm" variant="outline" onClick={() => setConfirm(null)}>
              Cancel
            </Button>
          </div>
        </div>
      )}
      {!system && admin && (
        <div className="space-y-1">
          <div className="flex flex-wrap items-end gap-2">
            <label className="flex flex-col gap-1">
              <span>Default schema</span>
              <input
                value={schema}
                onChange={(e) => setSchema(e.target.value)}
                className="h-8 w-40 border border-input bg-background px-2 text-sm"
              />
            </label>
            <Button
              size="sm"
              disabled={saveReason !== null}
              aria-describedby={saveReason ? reasonId : undefined}
              onClick={saveSchema}
            >
              Save default schema
            </Button>
          </div>
          {served && (
            <div>
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={grant}
                  aria-describedby={helpId}
                  onChange={(e) => setGrant(e.target.checked)}
                />
                <span>{schemaValid ? `Also grant existing objects in ${schema}` : "Also grant existing objects"}</span>
              </label>
              <p id={helpId} className="text-xs text-muted-foreground">
                Grants anon and authenticated the same access as when the data API was enabled. Nothing is revoked.
              </p>
            </div>
          )}
          {saveReason && (
            <p id={reasonId} className="text-xs text-muted-foreground">
              {saveReason}
            </p>
          )}
        </div>
      )}
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}

export interface DatabaseSelectorProps {
  databases: BranchDatabaseView[];
  /** `null` is every served database. */
  value: string[] | null;
  onChange: (value: string[] | null) => void;
}

/** "All served databases", or a multi-select of the branch's databases (not-served ones marked). */
export function DatabaseSelector({ databases, value, onChange }: DatabaseSelectorProps) {
  const options = selectable(databases);
  const all = value === null;
  return (
    <fieldset className="space-y-1">
      <legend className="sr-only">Databases</legend>
      <label className="flex items-center gap-2">
        <input
          type="checkbox"
          checked={all}
          onChange={(e) => onChange(e.target.checked ? null : options.filter((d) => d.data_api === "on").map((d) => d.name).slice(0, MAX_ALLOWED))}
        />
        <span>All served databases</span>
      </label>
      {!all &&
        options.map((d) => (
          <label key={d.name} className="flex items-center gap-2 pl-4">
            <input
              type="checkbox"
              checked={value.includes(d.name)}
              onChange={(e) => onChange(e.target.checked ? [...value, d.name] : value.filter((n) => n !== d.name))}
            />
            <span>{d.data_api === "on" ? d.name : `${d.name} (not served)`}</span>
          </label>
        ))}
    </fieldset>
  );
}

/** Whether a selector value can be sent: null, or 1 to 20 names. */
export const selectionReason = (value: string[] | null): string | null =>
  value === null || (value.length >= 1 && value.length <= MAX_ALLOWED)
    ? null
    : value.length === 0
      ? "Pick at least one database."
      : `Pick at most ${MAX_ALLOWED} databases.`;

export interface KeyDatabasesProps {
  databaseId: string;
  branchId: string;
  keyId: string;
  value: string[] | null;
  databases: BranchDatabaseView[];
}

/** A key's allow-list as chips, with an editor (both key kinds). */
export function KeyDatabases({ databaseId, branchId, keyId, value, databases }: KeyDatabasesProps) {
  // Changing a key is Developer's (updateKey).
  const canEdit = useProjectCaps().atLeast("developer");
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<string[] | null>(value);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const reasonId = useId();
  const reason = selectionReason(draft);

  async function save() {
    if (reason || busy) return;
    setBusy(true);
    setError(null);
    try {
      await updateKey(databaseId, branchId, keyId, { databases: draft });
      setEditing(false);
      await queryClient.invalidateQueries({ queryKey: getListKeysQueryKey(databaseId, branchId) });
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  const served = new Set(databases.filter((d) => d.data_api === "on").map((d) => d.name));
  return (
    <div className="space-y-1">
      <div className="flex flex-wrap items-center gap-1">
        {value === null ? (
          <span className="border px-2 py-0.5 text-xs">All served databases</span>
        ) : (
          value.map((n) => (
            <span key={n} className="border px-2 py-0.5 text-xs">
              {n}
              {databases.length > 0 && !served.has(n) && <span className="text-muted-foreground"> (not served)</span>}
            </span>
          ))
        )}
        {!editing && canEdit && (
          <Button
            size="xs"
            variant="outline"
            onClick={() => {
              setDraft(value);
              setEditing(true);
            }}
          >
            Edit databases
          </Button>
        )}
      </div>
      {editing && (
        <div className="space-y-2 border p-2">
          <DatabaseSelector databases={databases} value={draft} onChange={setDraft} />
          <div className="flex gap-2">
            <Button
              size="xs"
              disabled={reason !== null || busy}
              aria-describedby={reason ? reasonId : undefined}
              onClick={() => void save()}
            >
              Save databases
            </Button>
            <Button size="xs" variant="outline" onClick={() => setEditing(false)}>
              Cancel
            </Button>
          </div>
          {reason && (
            <p id={reasonId} className="text-xs text-muted-foreground">
              {reason}
            </p>
          )}
        </div>
      )}
      {error && (
        <p role="alert" className="text-xs text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}
