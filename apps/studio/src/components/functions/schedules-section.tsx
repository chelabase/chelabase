import { useId, useMemo, useReducer, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  createFunctionSchedule,
  deleteFunctionSchedule,
  getListFunctionSchedulesQueryKey,
  updateFunctionSchedule,
  useListFunctionSchedules,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { functionErrorText, useToggles } from "@/lib/functions";
import { initialScheduleForm, scheduleFormReason, scheduleFormReducer } from "@/lib/functions-schedules";
import { relativeTime, utc } from "@/lib/members";
import { ReadOnlyHint, VIEWER_REASON } from "./read-only-hint";

export interface SchedulesSectionProps {
  databaseId: string;
  branchId: string;
  name: string;
  canChange: boolean;
}

const field = "h-8 border border-input bg-ink px-2 font-mono text-sm disabled:opacity-50";

function timezones(): string[] {
  try {
    return (Intl as unknown as { supportedValuesOf?: (k: string) => string[] }).supportedValuesOf?.("timeZone") ?? [];
  } catch {
    return [];
  }
}

/** A function's cron schedules on the branch: list, add, edit (cron, time zone, on/off) and delete. */
export function SchedulesSection({ databaseId, branchId, name, canChange }: SchedulesSectionProps) {
  const queryClient = useQueryClient();
  const list = useListFunctionSchedules(databaseId, branchId, name);
  const toggles = useToggles(`${databaseId}/${branchId}/${name}/schedules`);
  const [form, dispatch] = useReducer(scheduleFormReducer, undefined, initialScheduleForm);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [deleting, setDeleting] = useState<string | null>(null);
  const zones = useMemo(timezones, []);
  const ids = { cron: useId(), tz: useId(), zones: useId(), reason: useId() };
  const reason = form.cron === "" ? null : scheduleFormReason(form);
  const viewer = canChange ? null : VIEWER_REASON;
  const refresh = () => queryClient.invalidateQueries({ queryKey: getListFunctionSchedulesQueryKey(databaseId, branchId, name) });

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
    if (busy || form.cron === "" || reason) return;
    const body = { cron: form.cron.trim(), timezone: form.timezone.trim() };
    void run(
      () => (form.editing ? updateFunctionSchedule(databaseId, branchId, name, form.editing, body) : createFunctionSchedule(databaseId, branchId, name, body)),
      () => dispatch({ type: "reset" }),
    );
  }

  const schedules = list.data ?? [];
  return (
    <div className="space-y-3 text-sm">
      {list.error ? (
        <p role="alert" className="text-coral">
          {functionErrorText(list.error)}
        </p>
      ) : !list.data ? (
        <p className="text-chalk-dim">Loading…</p>
      ) : schedules.length === 0 ? (
        <p className="text-chalk-dim">No schedules. A schedule calls the function on a cron, in the time zone you pick.</p>
      ) : (
        <ul className="divide-y divide-line border border-line">
          {schedules.map((s) => {
            const err = toggles.error(s.id);
            return (
              <li key={s.id} className="space-y-1 px-3 py-2">
                <div className="flex items-center gap-3">
                  <ReadOnlyHint reason={viewer}>
                    <Switch
                      checked={toggles.enabled(s.id, s.enabled)}
                      disabled={!canChange || toggles.pending(s.id)}
                      aria-label={`Schedule ${s.cron} enabled`}
                      onCheckedChange={(next) =>
                        void toggles.flip(s.id, next, () => updateFunctionSchedule(databaseId, branchId, name, s.id, { enabled: next }), refresh)
                      }
                    />
                  </ReadOnlyHint>
                  <div className="min-w-0 flex-1">
                    <p className="font-mono text-xs">{s.cron}</p>
                    <p className="text-xs text-chalk-dim">
                      {s.timezone}
                      {s.next_run_at && toggles.enabled(s.id, s.enabled) && (
                        <>
                          {" · next "}
                          <time dateTime={s.next_run_at} title={utc(s.next_run_at)}>
                            {relativeTime(s.next_run_at)}
                          </time>
                        </>
                      )}
                    </p>
                  </div>
                  <ReadOnlyHint reason={viewer}>
                    <Button size="xs" variant="ghost" disabled={!canChange || busy} aria-label={`Edit schedule ${s.cron}`} onClick={() => dispatch({ type: "edit", schedule: s })}>
                      Edit
                    </Button>
                  </ReadOnlyHint>
                  <ReadOnlyHint reason={viewer}>
                    <Button size="xs" variant="ghost" disabled={!canChange || busy} aria-label={`Delete schedule ${s.cron}`} onClick={() => setDeleting(s.id)}>
                      Delete
                    </Button>
                  </ReadOnlyHint>
                </div>
                {err && (
                  <p role="alert" className="text-xs text-coral">
                    {err}
                  </p>
                )}
                {deleting === s.id && (
                  <div className="flex items-center gap-2 pt-1">
                    <span className="text-xs">Delete this schedule?</span>
                    <Button size="xs" variant="destructive" disabled={busy} onClick={() => void run(() => deleteFunctionSchedule(databaseId, branchId, name, s.id), () => setDeleting(null))}>
                      Confirm delete
                    </Button>
                    <Button size="xs" variant="outline" onClick={() => setDeleting(null)}>
                      Cancel
                    </Button>
                  </div>
                )}
              </li>
            );
          })}
        </ul>
      )}

      <form onSubmit={submit} className="flex flex-wrap items-end gap-2">
        <label htmlFor={ids.cron} className="flex flex-col gap-1">
          <span className="text-xs text-chalk-dim">Cron</span>
          <input
            id={ids.cron}
            value={form.cron}
            onChange={(e) => dispatch({ type: "set", field: "cron", value: e.target.value })}
            disabled={!canChange}
            placeholder="*/15 * * * *"
            spellCheck={false}
            autoComplete="off"
            aria-invalid={reason !== null}
            aria-describedby={reason ? ids.reason : undefined}
            className={`${field} w-40`}
          />
        </label>
        <label htmlFor={ids.tz} className="flex flex-col gap-1">
          <span className="text-xs text-chalk-dim">Time zone</span>
          <input
            id={ids.tz}
            value={form.timezone}
            onChange={(e) => dispatch({ type: "set", field: "timezone", value: e.target.value })}
            disabled={!canChange}
            list={ids.zones}
            spellCheck={false}
            autoComplete="off"
            className={`${field} w-44`}
          />
          <datalist id={ids.zones}>
            {zones.map((z) => (
              <option key={z} value={z} />
            ))}
          </datalist>
        </label>
        <ReadOnlyHint reason={viewer}>
          <Button type="submit" size="sm" disabled={!canChange || busy || form.cron === "" || reason !== null}>
            {form.editing ? "Save schedule" : "Add schedule"}
          </Button>
        </ReadOnlyHint>
        {form.editing && (
          <Button type="button" size="sm" variant="outline" onClick={() => dispatch({ type: "reset" })}>
            Cancel
          </Button>
        )}
      </form>
      {reason && (
        <p id={ids.reason} className="text-xs text-chalk-dim">
          {reason}
        </p>
      )}
      {error && (
        <p role="alert" className="text-coral">
          {error}
        </p>
      )}
    </div>
  );
}
