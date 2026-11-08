import { useEffect, useId, useReducer, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  createFunctionTrigger,
  deleteFunctionTrigger,
  getListFunctionTriggersQueryKey,
  updateFunctionTrigger,
  useListBranchDatabases,
  useListFunctionTriggers,
  type FunctionTrigger,
  type FunctionTriggerSource,
} from "@chelabase/management-client";
import { Checkbox } from "@/components/table-editor/panel-bits";
import { Button } from "@/components/ui/button";
import { RadioGroup } from "@/components/ui/radio-group";
import { Select } from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { functionErrorText, useToggles } from "@/lib/functions";
import { STORAGE_EVENTS, TABLE_EVENTS, initialTriggerDraft, triggerDraftReducer, triggerReason, triggerRequest, triggerTarget } from "@/lib/functions-triggers";
import { relativeTime, utc } from "@/lib/members";
import { ReadOnlyHint, VIEWER_REASON } from "./read-only-hint";

export interface TriggersSectionProps {
  databaseId: string;
  branchId: string;
  name: string;
  canChange: boolean;
}

export const AWAKE_NOTE = "The branch's compute stays awake while table triggers are installed and enabled.";

const field = "h-8 border border-input bg-ink px-2 font-mono text-sm disabled:opacity-50";

const STATE_TONE: Record<string, string> = { installed: "border-seafoam/60 text-seafoam", failed: "border-coral/60 text-coral" };

function TriggerRow({ t, canChange, busy, toggles, onToggle, onDelete }: {
  t: FunctionTrigger;
  canChange: boolean;
  busy: boolean;
  toggles: ReturnType<typeof useToggles>;
  onToggle: (next: boolean) => void;
  onDelete: () => void;
}) {
  const viewer = canChange ? null : VIEWER_REASON;
  const err = toggles.error(t.id);
  const target = triggerTarget(t);
  return (
    <li className="space-y-1.5 px-3 py-2">
      <div className="flex items-start gap-3">
        <ReadOnlyHint reason={viewer}>
          <Switch checked={toggles.enabled(t.id, t.enabled)} disabled={!canChange || toggles.pending(t.id)} aria-label={`Trigger on ${target} enabled`} onCheckedChange={onToggle} />
        </ReadOnlyHint>
        <div className="min-w-0 flex-1 space-y-0.5">
          <p className="flex flex-wrap items-center gap-2">
            <span className="border border-line-strong px-1.5 text-xs">{t.source}</span>
            <span className="min-w-0 truncate font-mono text-xs">{target}</span>
            <span className={`border px-1.5 text-xs ${STATE_TONE[t.state] ?? "border-line-strong text-chalk-dim"}`}>{t.state.replace("_", " ")}</span>
          </p>
          <p className="font-mono text-xs text-chalk-dim">{t.events.join(", ")}</p>
          <p className="text-xs text-chalk-dim">
            {t.last_delivered_at ? (
              <>
                Last delivered{" "}
                <time dateTime={t.last_delivered_at} title={utc(t.last_delivered_at)}>
                  {relativeTime(t.last_delivered_at)}
                </time>
              </>
            ) : (
              "Nothing delivered yet"
            )}
            {" · "}
            <span className={t.dead_count > 0 ? "text-coral" : undefined}>{t.dead_count} dead</span>
            {" · "}
            {t.dropped_count} dropped
          </p>
        </div>
        <ReadOnlyHint reason={viewer}>
          <Button size="xs" variant="ghost" disabled={!canChange || busy} aria-label={`Delete trigger on ${target}`} onClick={onDelete}>
            Delete
          </Button>
        </ReadOnlyHint>
      </div>
      {t.last_error && <p className="font-mono text-xs break-all text-coral">Install failed: {t.last_error}</p>}
      {t.last_delivery_error && <p className="font-mono text-xs break-all text-coral">Last delivery failed: {t.last_delivery_error}</p>}
      {err && (
        <p role="alert" className="text-xs text-coral">
          {err}
        </p>
      )}
    </li>
  );
}

/** A function's table and storage triggers on the branch: list, add, turn on or off, delete. */
export function TriggersSection({ databaseId, branchId, name, canChange }: TriggersSectionProps) {
  const queryClient = useQueryClient();
  const list = useListFunctionTriggers(databaseId, branchId, name);
  const dbs = useListBranchDatabases(databaseId, branchId);
  const main = dbs.data?.find((d) => d.main)?.name ?? dbs.data?.[0]?.name ?? "";
  const toggles = useToggles(`${databaseId}/${branchId}/${name}/triggers`);
  const [draft, dispatch] = useReducer(triggerDraftReducer, main, initialTriggerDraft);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const ids = { db: useId(), table: useId(), bucket: useId(), prefix: useId(), reason: useId() };
  const viewer = canChange ? null : VIEWER_REASON;
  const reason = triggerReason(draft);
  // The databases arrive after the first render: start on the main one.
  useEffect(() => {
    if (draft.pgDatabase === "" && main !== "") dispatch({ type: "set", field: "pgDatabase", value: main });
  }, [main, draft.pgDatabase]);

  const refresh = () => queryClient.invalidateQueries({ queryKey: getListFunctionTriggersQueryKey(databaseId, branchId, name) });

  async function run(action: () => Promise<unknown>, after?: () => void) {
    setBusy(true);
    setError(null);
    try {
      await action();
      after?.();
      await refresh();
    } catch (err) {
      setError(functionErrorText(err));
    } finally {
      setBusy(false);
    }
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || reason) return;
    void run(() => createFunctionTrigger(databaseId, branchId, name, triggerRequest(draft)), () => dispatch({ type: "reset" }));
  }

  const triggers = list.data ?? [];
  const events = draft.source === "table" ? TABLE_EVENTS : STORAGE_EVENTS;
  return (
    <div className="space-y-3 text-sm">
      <p className="text-xs text-chalk-dim">{AWAKE_NOTE}</p>
      {list.error ? (
        <p role="alert" className="text-coral">
          {functionErrorText(list.error)}
        </p>
      ) : !list.data ? (
        <p className="text-chalk-dim">Loading…</p>
      ) : triggers.length === 0 ? (
        <p className="text-chalk-dim">No triggers. A trigger calls the function when rows change in a table or objects change in storage.</p>
      ) : (
        <ul className="divide-y divide-line border border-line">
          {triggers.map((t) => (
            <TriggerRow
              key={t.id}
              t={t}
              canChange={canChange}
              busy={busy}
              toggles={toggles}
              onToggle={(next) => void toggles.flip(t.id, next, () => updateFunctionTrigger(databaseId, branchId, name, t.id, { enabled: next }), refresh)}
              onDelete={() => setDeleting(t.id)}
            />
          ))}
        </ul>
      )}
      {deleting && (
        <div className="flex flex-wrap items-center gap-2 border border-line p-3">
          <span>Delete this trigger? Its queued events are dropped.</span>
          <Button size="sm" variant="destructive" disabled={busy} onClick={() => void run(() => deleteFunctionTrigger(databaseId, branchId, name, deleting), () => setDeleting(null))}>
            Confirm delete
          </Button>
          <Button size="sm" variant="outline" onClick={() => setDeleting(null)}>
            Cancel
          </Button>
        </div>
      )}

      <form onSubmit={submit} className="space-y-3 border border-line p-3">
        <RadioGroup<FunctionTriggerSource>
          label="New trigger on"
          layout="segmented"
          options={[
            { value: "table", label: "Table" },
            { value: "storage", label: "Storage" },
          ]}
          value={draft.source}
          onValueChange={(source) => dispatch({ type: "source", source })}
          disabled={!canChange}
        />
        <div className="flex flex-wrap items-end gap-2">
          <label htmlFor={ids.db} className="flex flex-col gap-1">
            <span className="text-xs text-chalk-dim">Postgres database</span>
            <Select id={ids.db} value={draft.pgDatabase} disabled={!canChange || !dbs.data} onChange={(e) => dispatch({ type: "set", field: "pgDatabase", value: e.target.value })} wrapperClassName="w-40">
              {!dbs.data && <option value="">Loading…</option>}
              {dbs.data?.map((d) => (
                <option key={d.name} value={d.name}>
                  {d.name}
                </option>
              ))}
            </Select>
          </label>
          {draft.source === "table" ? (
            <label htmlFor={ids.table} className="flex flex-col gap-1">
              <span className="text-xs text-chalk-dim">Table</span>
              <input id={ids.table} value={draft.table} onChange={(e) => dispatch({ type: "set", field: "table", value: e.target.value })} disabled={!canChange} placeholder="public.orders" spellCheck={false} autoComplete="off" className={`${field} w-48`} />
            </label>
          ) : (
            <>
              <label htmlFor={ids.bucket} className="flex flex-col gap-1">
                <span className="text-xs text-chalk-dim">Bucket (optional)</span>
                <input id={ids.bucket} value={draft.bucket} onChange={(e) => dispatch({ type: "set", field: "bucket", value: e.target.value })} disabled={!canChange} placeholder="every bucket" spellCheck={false} autoComplete="off" className={`${field} w-36`} />
              </label>
              <label htmlFor={ids.prefix} className="flex flex-col gap-1">
                <span className="text-xs text-chalk-dim">Prefix (optional)</span>
                <input id={ids.prefix} value={draft.prefix} onChange={(e) => dispatch({ type: "set", field: "prefix", value: e.target.value })} disabled={!canChange} placeholder="every key" spellCheck={false} autoComplete="off" className={`${field} w-36`} />
              </label>
            </>
          )}
        </div>
        <fieldset className="flex flex-wrap gap-x-4 gap-y-1">
          <legend className="mb-1 text-xs text-chalk-dim">Events</legend>
          {events.map((ev) => (
            <label key={ev} className="flex items-center gap-1.5 font-mono text-xs">
              <Checkbox checked={draft.events.includes(ev)} disabled={!canChange} onChange={() => dispatch({ type: "toggle-event", event: ev })} />
              {ev}
            </label>
          ))}
        </fieldset>
        <div className="flex items-center gap-3">
          <ReadOnlyHint reason={viewer}>
            <Button type="submit" size="sm" disabled={!canChange || busy || reason !== null} aria-describedby={reason ? ids.reason : undefined}>
              Add trigger
            </Button>
          </ReadOnlyHint>
          {reason && canChange && (
            <span id={ids.reason} className="text-xs text-chalk-dim">
              {reason}
            </span>
          )}
        </div>
      </form>
      {error && (
        <p role="alert" className="text-coral">
          {error}
        </p>
      )}
    </div>
  );
}
