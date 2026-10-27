import { useId, useState } from "react";
import { useListKeys, useListRoleSessions, type Merge, type RoleSessions, type UserStepBody } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import {
  PHASES,
  PHASE_LABEL,
  ackIds,
  deleteUserStep,
  insertUserStep,
  isFlagged,
  moveUserStep,
  stepErrors,
  utcMinute,
  type Plan,
  type PlanStep,
} from "@/lib/merges";
import { OWNER_LEVEL_TRAITS } from "@/lib/console";
import { errorText } from "@/lib/errors";
import { Badge } from "./merge-changes";
import type { SaveMerge } from "./merge-page";

export interface MergePlanProps {
  databaseId: string;
  targetName: string;
  merge: Merge;
  plan: Plan | null;
  editable: boolean;
  save: SaveMerge;
}

const DEFAULT_LOCK_TIMEOUT_MS = 5000;
const LOCK_TIMEOUT_RANGE = [100, 600_000] as const;
const KIND_LABEL: Record<PlanStep["kind"], string> = { generated: "Generated", user: "Your step", own_sql: "Own SQL" };
const LOSS_TEXT = "Loses data: the dropped or rewritten values can't be recovered.";
const SECURITY_TEXT = `Changes security: a role that bypasses row-level security can read and change every row, whatever the policies say, and an owner-level role (${OWNER_LEVEL_TRAITS}) reaches even more.`;

function ackLabel(s: PlanStep): string {
  const parts = [s.data_loss && "data loss", s.security && "security change", s.breaking && "breaking change"].filter(
    (p): p is string => typeof p === "string",
  );
  const what = parts.length > 1 ? `${parts.slice(0, -1).join(", ")} and ${parts[parts.length - 1]}` : parts[0];
  return `I accept the ${what} of ${s.id}`;
}

/**
 * What a role rename touches (E29a): the data API's secret keys that use the role follow it
 * (the control plane moves them with the role); direct connections by name break. The sessions
 * open as the role on the target right now are listed; clients not connected now can't be, so the
 * text names what to change instead.
 */
function RenameImpact({ databaseId, branchId, role, newName, targetName }: { databaseId: string; branchId: string; role: string; newName: string; targetName: string }) {
  const keys = useListKeys(databaseId, branchId);
  const sessions = useListRoleSessions(databaseId, branchId, role);
  const using = (keys.data ?? []).filter((k) => k.kind === "secret" && k.role === role);
  return (
    <div className="space-y-1 text-destructive">
      <p>
        Renames role {role} to {newName}: anything that connects to {targetName} as {role} will fail to authenticate after the
        apply. Clients that aren&apos;t connected right now aren&apos;t listed: change every connection string that names {role} to{" "}
        {newName}.
      </p>
      {keys.data !== undefined && (
        <p className="text-foreground">
          {using.length > 0
            ? `Secret keys that use ${role}: ${using.map((k) => k.name).join(", ")}. They move with the role and keep working.`
            : `No secret key uses ${role}.`}
        </p>
      )}
      {sessions.data !== undefined && <LiveSessions sessions={sessions.data} role={role} targetName={targetName} />}
      {sessions.error && (
        <p className="text-foreground">The live sessions couldn&apos;t be read: {errorText(sessions.error)}</p>
      )}
    </div>
  );
}

/** The sessions open as `role` on the target right now (`listRoleSessions`). */
function LiveSessions({ sessions, role, targetName }: { sessions: RoleSessions; role: string; targetName: string }) {
  if (!sessions.compute_running) {
    return <p className="text-foreground">{`${targetName}'s compute isn't running, so no one is connected as ${role} right now.`}</p>;
  }
  if (sessions.count === 0) return <p className="text-foreground">{`No one is connected to ${targetName} as ${role} right now.`}</p>;
  return (
    <div className="space-y-1 text-foreground">
      <p>{`Connected to ${targetName} as ${role} right now: ${sessions.count} ${sessions.count === 1 ? "session" : "sessions"}.`}</p>
      <ul aria-label={`Sessions connected to ${targetName} as ${role}`} className="list-disc pl-5">
        {sessions.sessions.map((s, i) => (
          <li key={`${s.client_addr ?? "local"}-${s.backend_start}-${i}`}>
            {s.client_addr ?? "inside the compute"} · {s.application_name || "no application name"} · since {utcMinute(s.backend_start)}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** The user step a plan row belongs to, when it is one. */
function userStepOf(row: PlanStep, steps: UserStepBody[]): UserStepBody | undefined {
  if (row.kind !== "user") return undefined;
  return steps
    .filter((s) => row.id === s.id || row.id.startsWith(`${s.id}.`))
    .sort((a, b) => b.id.length - a.id.length)[0];
}

/** Steps by phase, with acknowledgements, own steps (insert, move, delete) and the lock timeout. */
export function MergePlan({ databaseId, targetName, merge, plan, editable, save }: MergePlanProps) {
  const [acks, setAcks] = useState<Set<string>>(() => new Set(merge.acknowledgements));
  const [anchor, setAnchor] = useState("");
  const [sql, setSql] = useState("");
  const [hint, setHint] = useState<string | null>(null);
  const savedTimeout = typeof merge.options.lock_timeout_ms === "number" ? merge.options.lock_timeout_ms : DEFAULT_LOCK_TIMEOUT_MS;
  const [timeout, setTimeout_] = useState(String(savedTimeout));
  const [timeoutHint, setTimeoutHint] = useState<string | null>(null);
  // The server's acknowledgements and lock timeout: when either changes (this section's save, or
  // someone else's), the local copy starts again from it; the step SQL being written stays.
  const serverAcks = JSON.stringify(merge.acknowledgements);
  const [ackBase, setAckBase] = useState(serverAcks);
  if (ackBase !== serverAcks) {
    setAckBase(serverAcks);
    setAcks(new Set(merge.acknowledgements));
  }
  const [timeoutBase, setTimeoutBase] = useState(savedTimeout);
  if (timeoutBase !== savedTimeout) {
    setTimeoutBase(savedTimeout);
    setTimeout_(String(savedTimeout));
  }
  const anchorId = useId();
  const sqlId = useId();
  const timeoutId = useId();
  const timeoutHintId = useId();

  const rows = plan?.steps ?? [];
  const flagged = rows.filter(isFlagged);
  const errors = stepErrors(merge.error, merge.steps.map((s) => s.id));
  // What a step can follow: every step of the plan once (a user step by its own id).
  const anchors: string[] = [];
  for (const row of rows) {
    const id = userStepOf(row, merge.steps)?.id ?? row.id;
    if (!anchors.includes(id)) anchors.push(id);
  }
  if (!plan) for (const s of merge.steps) anchors.push(s.id);

  const toggle = (s: PlanStep, on: boolean) =>
    setAcks((prev) => {
      const next = new Set(prev);
      for (const id of ackIds(s)) (on ? next.add(id) : next.delete(id));
      return next;
    });
  function saveAcks() {
    // In plan order; acknowledgements of items no step flags now are kept as they were.
    const flaggedIds = flagged.flatMap(ackIds);
    const inPlan = flaggedIds.filter((id) => acks.has(id));
    const others = [...acks].filter((id) => !flaggedIds.includes(id));
    void save({ acknowledgements: [...new Set([...inPlan, ...others])] });
  }
  function addStep() {
    if (sql.trim() === "") {
      setHint("Write the step's SQL first.");
      return;
    }
    setHint(null);
    void save({ steps: insertUserStep(merge.steps, anchor === "" ? null : anchor, sql.trim()) });
  }
  function move(id: string, dir: "up" | "down") {
    const next = moveUserStep(merge.steps, rows, id, dir);
    if (next) void save({ steps: next });
  }
  function saveTimeout() {
    const n = /^\d+$/.test(timeout.trim()) ? Number(timeout.trim()) : NaN;
    if (Number.isNaN(n) || n < LOCK_TIMEOUT_RANGE[0] || n > LOCK_TIMEOUT_RANGE[1]) {
      setTimeoutHint("The lock timeout must be a whole number between 100 and 600000 milliseconds.");
      return;
    }
    setTimeoutHint(null);
    void save({ options: { lock_timeout_ms: n } });
  }

  const field = "rounded-lg border border-input bg-background px-2 text-sm";
  const shownUserSteps = new Set<string>();

  return (
    <div className="space-y-4 text-sm">
      {plan === null ? (
        <p className="text-muted-foreground">
          {merge.status === "computing" ? "Planning…" : "No plan yet: every conflict needs a choice, saved, before there is one."}
        </p>
      ) : rows.length === 0 ? (
        <p className="text-muted-foreground">Nothing to change: the target already has the merged schema.</p>
      ) : (
        PHASES.filter((p) => rows.some((s) => s.phase === p)).map((phase) => (
          <div key={phase} className="space-y-2">
            <h3 className="font-medium">{PHASE_LABEL[phase]}</h3>
            <ol className="space-y-2">
              {rows
                .filter((s) => s.phase === phase)
                .map((s) => {
                  const own = userStepOf(s, merge.steps);
                  const firstOfOwn = own !== undefined && !shownUserSteps.has(own.id);
                  if (own) shownUserSteps.add(own.id);
                  const acked = ackIds(s).every((id) => acks.has(id));
                  return (
                    <li key={s.id} className="space-y-2 rounded-lg border p-3">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-mono [overflow-wrap:anywhere]">{s.id}</span>
                        <Badge>{KIND_LABEL[s.kind] ?? s.kind}</Badge>
                        {s.data_loss && <Badge hazard>Loses data</Badge>}
                        {s.security && <Badge hazard>Security</Badge>}
                        {s.breaking && <Badge hazard>Breaks clients</Badge>}
                        {s.rewrite && <Badge>Rewrites the table</Badge>}
                        {s.locks.length > 0 && (
                          <span className="text-xs text-muted-foreground">Locks: {s.locks.join(", ")}</span>
                        )}
                      </div>
                      <pre
                        tabIndex={0}
                        aria-label={`SQL of ${s.id}`}
                        className="max-h-48 overflow-auto rounded-lg border bg-background p-2 font-mono text-xs whitespace-pre-wrap"
                      >
                        {s.sql}
                      </pre>
                      {s.data_loss && <p className="text-destructive">{LOSS_TEXT}</p>}
                      {s.security && <p className="text-destructive">{SECURITY_TEXT}</p>}
                      {s.breaking?.role_rename && (
                        <RenameImpact
                          databaseId={databaseId}
                          branchId={merge.target_branch_id}
                          role={s.breaking.role_rename.role}
                          newName={s.breaking.role_rename.new_name}
                          targetName={targetName}
                        />
                      )}
                      {isFlagged(s) && (
                        <label className="flex items-center gap-2">
                          <input
                            type="checkbox"
                            checked={acked}
                            disabled={!editable}
                            onChange={(e) => toggle(s, e.target.checked)}
                          />
                          {ackLabel(s)}
                        </label>
                      )}
                      {own && firstOfOwn && (
                        <div className="flex flex-wrap gap-2">
                          <Button
                            size="xs"
                            variant="outline"
                            aria-label={`Move ${own.id} up`}
                            disabled={!editable || moveUserStep(merge.steps, rows, own.id, "up") === null}
                            onClick={() => move(own.id, "up")}
                          >
                            Move up
                          </Button>
                          <Button
                            size="xs"
                            variant="outline"
                            aria-label={`Move ${own.id} down`}
                            disabled={!editable || moveUserStep(merge.steps, rows, own.id, "down") === null}
                            onClick={() => move(own.id, "down")}
                          >
                            Move down
                          </Button>
                          <Button
                            size="xs"
                            variant="outline"
                            aria-label={`Delete ${own.id}`}
                            disabled={!editable}
                            onClick={() => void save({ steps: deleteUserStep(merge.steps, own.id) })}
                          >
                            Delete
                          </Button>
                        </div>
                      )}
                    </li>
                  );
                })}
            </ol>
          </div>
        ))
      )}

      {flagged.length > 0 && (
        <div className="space-y-1">
          <Button size="sm" variant="outline" disabled={!editable} onClick={saveAcks}>
            Save acknowledgements
          </Button>
          {merge.status === "rehearsed" && (
            <p className="text-xs text-muted-foreground">
              Saving acknowledgements keeps the rehearsal. Changing the steps or the lock timeout returns the merge to draft.
            </p>
          )}
        </div>
      )}

      {plan === null && merge.steps.length > 0 && (
        <div className="space-y-2">
          <h3 className="font-medium">Your steps</h3>
          <ul className="space-y-2">
            {merge.steps.map((s) => (
              <li key={s.id} aria-label={`Your step ${s.id}`} className="space-y-2 rounded-lg border p-3">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono [overflow-wrap:anywhere]">{s.id}</span>
                  <span className="text-xs text-muted-foreground">
                    {s.after ? (
                      <>
                        after <span className="font-mono [overflow-wrap:anywhere]">{s.after}</span>
                      </>
                    ) : (
                      "at the start"
                    )}
                  </span>
                  <Button
                    size="xs"
                    variant="outline"
                    aria-label={`Delete ${s.id}`}
                    disabled={!editable}
                    onClick={() => void save({ steps: deleteUserStep(merge.steps, s.id) })}
                  >
                    Delete
                  </Button>
                </div>
                <pre
                  tabIndex={0}
                  aria-label={`SQL of your step ${s.id}`}
                  className="max-h-48 overflow-auto rounded-lg border bg-background p-2 font-mono text-xs whitespace-pre-wrap"
                >
                  {s.sql}
                </pre>
                {errors[s.id] && (
                  <div role="alert" className="text-destructive">
                    {errors[s.id].map((e) => (
                      <p key={e}>{e}</p>
                    ))}
                  </div>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="space-y-2 rounded-lg border p-3">
        <h3 className="font-medium">Add your own step</h3>
        <p className="text-xs text-muted-foreground">
          SQL of your own (a backfill before a NOT NULL, say) runs right after the step you pick, in the phase its
          statements need. Role and database changes and transaction control can&apos;t go in a step.
        </p>
        <div className="flex flex-col gap-1">
          <label htmlFor={anchorId}>Insert after</label>
          <select
            id={anchorId}
            value={anchor}
            disabled={!editable}
            onChange={(e) => setAnchor(e.target.value)}
            className={`${field} h-8 w-72`}
          >
            <option value="">At the start</option>
            {anchors.map((id) => (
              <option key={id} value={id}>
                {id}
              </option>
            ))}
          </select>
        </div>
        <div className="flex flex-col gap-1">
          <label htmlFor={sqlId}>Step SQL</label>
          <textarea
            id={sqlId}
            value={sql}
            disabled={!editable}
            onChange={(e) => setSql(e.target.value)}
            rows={3}
            className={`${field} py-1 font-mono text-xs`}
          />
        </div>
        {hint && (
          <p role="alert" className="text-destructive">
            {hint}
          </p>
        )}
        <Button size="sm" variant="outline" disabled={!editable} onClick={addStep}>
          Add step
        </Button>
      </div>

      <div className="flex flex-wrap items-end gap-2">
        <div className="flex flex-col gap-1">
          <label htmlFor={timeoutId}>Lock timeout (ms)</label>
          <input
            id={timeoutId}
            type="number"
            min={LOCK_TIMEOUT_RANGE[0]}
            max={LOCK_TIMEOUT_RANGE[1]}
            value={timeout}
            disabled={!editable}
            onChange={(e) => {
              setTimeout_(e.target.value);
              setTimeoutHint(null);
            }}
            aria-invalid={timeoutHint ? true : undefined}
            aria-describedby={timeoutHint ? timeoutHintId : undefined}
            className={`${field} h-8 w-32`}
          />
        </div>
        <Button size="sm" variant="outline" disabled={!editable} onClick={saveTimeout}>
          Save lock timeout
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        How long the plan&apos;s transaction waits for a lock before it gives up, rolling everything back.
      </p>
      {timeoutHint && (
        <p id={timeoutHintId} className="text-destructive">
          {timeoutHint}
        </p>
      )}
    </div>
  );
}
