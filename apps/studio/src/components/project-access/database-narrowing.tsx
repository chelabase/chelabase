import { useEffect, useId, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  deleteGrantOverride,
  getListProjectGrantsQueryKey,
  putGrantOverride,
  type DatabaseSummary,
  type GrantOverride,
  type PlatformRole,
  type Project,
} from "@chelabase/management-client";
import { PlatformRoleField } from "@/components/members/platform-role-field";
import { refreshAccess } from "@/components/members/keys";
import { Button } from "@/components/ui/button";
import { ROLE_LABEL } from "@/lib/members";
import { refusal } from "@/lib/role-rules";
import { useRoleSources } from "./use-role-sources";
import { failureOf, FailureText, type Failure } from "./failure";
import { narrowsGrant, overrideWords } from "./narrowing";

export interface NarrowingGrant {
  platformRole: PlatformRole;
  pgRoles: string[];
}

export interface DatabaseNarrowingProps {
  project: Project;
  database: DatabaseSummary;
  memberId: string;
  memberName: string;
  grant: NarrowingGrant;
  override: GrantOverride | undefined;
  /** role → what it also gives, to show next to the checkboxes. */
  chains: ReadonlyMap<string, string[]>;
  /** The grant was refused because of what's left of it on this database. */
  flagged: boolean;
  onStatus: (message: string) => void;
}

const RANK: Record<PlatformRole, number> = { viewer: 0, developer: 1, admin: 2 };

/** Square, hairline, sand when checked (no native rounded box). */
const checkbox =
  "size-4 shrink-0 cursor-default appearance-none rounded-none border border-input bg-ink outline-none checked:border-sand checked:bg-sand checked:shadow-[inset_0_0_0_3px_var(--color-ink)] focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand";

/**
 * One line of the editor's "Per database" part: the database, then either "Same as the project"
 * and a Narrow button, or the narrowing in words with Change and Stop narrowing. The form offers
 * only narrower choices (a lower role, fewer roles); the API still judges.
 */
export function DatabaseNarrowing({ project, database, memberId, memberName, grant, override, chains, flagged, onStatus }: DatabaseNarrowingProps) {
  const queryClient = useQueryClient();
  const [editing, setEditing] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<Failure | null>(null);

  /** The line's Change / Narrow button: focus returns here after a save or "Stop narrowing". */
  const primary = useRef<HTMLButtonElement>(null);
  const [refocus, setRefocus] = useState(false);
  useEffect(() => {
    if (refocus && !busy) {
      primary.current?.focus();
      setRefocus(false);
    }
  }, [refocus, busy]);

  const refresh = (): Promise<void> => {
    const grants = queryClient.invalidateQueries({ queryKey: getListProjectGrantsQueryKey(project.id) });
    refreshAccess(queryClient);
    return grants;
  };

  async function stop() {
    setBusy(true);
    setFailure(null);
    try {
      await deleteGrantOverride(project.id, memberId, { database_id: database.database_id });
      onStatus(`${memberName} is no longer narrowed on ${database.name}.`);
    } catch (err) {
      setFailure(failureOf(err, project));
    } finally {
      await refresh();
      setBusy(false);
      setRefocus(true);
    }
  }

  return (
    <div role="group" aria-label={database.name} className="space-y-2 py-2">
      <div className="flex min-h-8 flex-wrap items-center gap-x-3 gap-y-1 text-sm">
        <span className="w-32 shrink-0 font-medium">{database.name}</span>
        {override ? (
          <>
            <span className="text-chalk">{overrideWords(override)}</span>
            <Button ref={primary} variant="outline" size="sm" aria-label={`Change narrowing on ${database.name}`} disabled={busy} onClick={() => setEditing((e) => !e)}>
              Change
            </Button>
            <Button variant="ghost" size="sm" aria-label={`Stop narrowing on ${database.name}`} disabled={busy} onClick={() => void stop()}>
              Stop narrowing
            </Button>
          </>
        ) : (
          <>
            <span className="text-chalk-dim">Same as the project</span>
            <Button ref={primary} variant="outline" size="sm" aria-label={`Narrow access on ${database.name}`} disabled={busy} onClick={() => setEditing((e) => !e)}>
              Narrow
            </Button>
          </>
        )}
      </div>
      {flagged && (
        <p role="alert" className="text-sm text-coral">
          What's left of the grant on {database.name} is why the grant was refused. Change or stop the narrowing here.
        </p>
      )}
      {failure && <FailureText failure={failure} />}
      {editing && (
        <NarrowingForm
          project={project}
          database={database}
          memberId={memberId}
          memberName={memberName}
          grant={grant}
          override={override}
          chains={chains}
          onCancel={() => setEditing(false)}
          onSaved={async (message) => {
            setEditing(false);
            onStatus(message);
            await refresh();
            setRefocus(true);
          }}
          onFailure={setFailure}
          onSettled={refresh}
        />
      )}
    </div>
  );
}

function NarrowingForm({
  project,
  database,
  memberId,
  memberName,
  grant,
  override,
  chains,
  onCancel,
  onSaved,
  onFailure,
  onSettled,
}: Pick<DatabaseNarrowingProps, "project" | "database" | "memberId" | "memberName" | "grant" | "override" | "chains"> & {
  onCancel: () => void;
  onSaved: (message: string) => Promise<void>;
  onFailure: (f: Failure | null) => void;
  onSettled: () => void;
}) {
  const id = useId();
  // "Same as the project" is the first choice for the role and for the roles.
  const [sameRole, setSameRole] = useState(override?.platform_role == null);
  // A stored role above the grant's (the grant was lowered since) is shown clamped, with a note.
  const stored = override?.platform_role ?? null;
  const clamped = stored !== null && RANK[stored] > RANK[grant.platformRole];
  const [role, setRole] = useState<PlatformRole>(clamped ? grant.platformRole : (stored ?? grant.platformRole));
  const [allRoles, setAllRoles] = useState(override?.pg_roles == null);
  const [chosen, setChosen] = useState<string[]>(override?.pg_roles ?? grant.pgRoles);
  const [busy, setBusy] = useState(false);

  const draft = {
    platformRole: sameRole || role === grant.platformRole ? null : role,
    pgRoles: allRoles ? null : grant.pgRoles.filter((r) => chosen.includes(r)),
  };
  const narrower = narrowsGrant(grant, draft);
  const effective = draft.platformRole ?? grant.platformRole;

  // Below Admin the server refuses roles whose chain is owner-level, LOGIN or reserved on this
  // database (`check_effective`): while such a role is still held, the lower roles stay locked.
  const { sources } = useRoleSources([database]);
  const rows = sources[0]?.roles ?? [];
  const held = allRoles ? grant.pgRoles : grant.pgRoles.filter((r) => chosen.includes(r));
  const adminOnly = grant.platformRole === "admin" ? held.filter((r) => refusal(rows, r) !== null) : [];
  const locked: PlatformRole[] = adminOnly.length > 0 ? ["viewer", "developer"] : [];

  async function save() {
    setBusy(true);
    onFailure(null);
    try {
      await putGrantOverride(project.id, memberId, {
        database_id: database.database_id,
        platform_role: draft.platformRole,
        pg_roles: draft.pgRoles,
      });
      await onSaved(`Narrowed ${memberName} on ${database.name}.`);
    } catch (err) {
      onFailure(failureOf(err, project));
      onSettled();
    } finally {
      setBusy(false);
    }
  }

  return (
    <form
      aria-label={`Narrow on ${database.name}`}
      onSubmit={(e) => {
        e.preventDefault();
        if (narrower && !busy) void save();
      }}
      className="space-y-4 border border-line p-3"
    >
      <fieldset disabled={busy} className="m-0 min-w-0 space-y-4 border-0 p-0">
        <div className="space-y-2">
          <label className="flex items-center gap-2 text-sm">
            <input type="checkbox" className={checkbox} checked={sameRole} onChange={(e) => setSameRole(e.target.checked)} />
            Same as the project
          </label>
          {!sameRole && (
            <PlatformRoleField value={role} onChange={setRole} max={grant.platformRole} locked={locked} label="Platform role on this database" />
          )}
          {!sameRole && adminOnly.length > 0 && (
            <p className="text-xs text-chalk-dim">
              Keep Admin while {adminOnly.join(", ")} {adminOnly.length > 1 ? "are" : "is"} chosen: below Admin{" "}
              {adminOnly.length > 1 ? "they" : "it"} can't be held on {database.name}. Narrow the roles first.
            </p>
          )}
          {!sameRole && clamped && (
            <p className="text-xs text-chalk-dim">
              This narrowing was {ROLE_LABEL[stored]}, above the grant's {ROLE_LABEL[grant.platformRole]}; it's shown as {ROLE_LABEL[grant.platformRole]}.
            </p>
          )}
          {grant.platformRole === "admin" && effective !== "admin" && (
            <p className="text-xs text-chalk-dim">Below Admin, {memberName} can't act as the database owner on {database.name}.</p>
          )}
        </div>
        {grant.pgRoles.length > 0 && (
          <div className="space-y-2">
            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" className={checkbox} checked={allRoles} onChange={(e) => setAllRoles(e.target.checked)} />
              All of the project's roles
            </label>
            {!allRoles && (
              <fieldset className="m-0 min-w-0 space-y-1 border-0 p-0">
                <legend className="mb-1 text-sm font-medium">Roles on this database</legend>
                {grant.pgRoles.map((r) => {
                  const chain = chains.get(r) ?? [];
                  return (
                    <label key={r} htmlFor={`${id}-${r}`} className="flex items-center gap-2 text-sm">
                      <input
                        id={`${id}-${r}`}
                        type="checkbox"
                        className={checkbox}
                        checked={chosen.includes(r)}
                        onChange={(e) => setChosen(e.target.checked ? [...chosen, r] : chosen.filter((c) => c !== r))}
                      />
                      <span className="font-mono text-[13px]">{r}</span>
                      {chain.length > 0 && <span className="text-chalk-dim">+ {chain.join(", ")}</span>}
                    </label>
                  );
                })}
              </fieldset>
            )}
          </div>
        )}
      </fieldset>
      <div className="flex gap-2">
        <Button type="submit" disabled={!narrower || busy}>
          {busy ? "Saving…" : "Save narrowing"}
        </Button>
        <Button type="button" variant="ghost" disabled={busy} onClick={onCancel}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
