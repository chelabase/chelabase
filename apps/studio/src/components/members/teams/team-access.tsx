import { useCallback, useEffect, useId, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  deleteTeamGrant,
  putTeamGrant,
  type MeResponse,
  type PlatformRole,
  type Project,
  type Team,
  type TeamProjectAccess,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Select } from "@/components/ui/select";
import { errorText } from "@/lib/errors";
import { ROLE_LABEL } from "@/lib/members";
import { cn } from "@/lib/utils";
import { failureOf, FailureText, type Failure } from "@/components/project-access/failure";
import { useRoleSources } from "@/components/project-access/use-role-sources";
import { AdminOnlyNote, adminOnlyChecked, escapeName, RoleChecklist } from "../hub";
import { PlatformRoleField } from "../platform-role-field";
import { roleChoices } from "../role-picker";
import { refreshTeams } from "./keys";
import { teamName } from "./team-model";

export interface TeamAccessProps {
  team: Team;
  me: MeResponse;
  /** Tenant admins edit; everyone else reads (Part 7). */
  canEdit: boolean;
  onDirtyChange: (projectId: string, dirty: boolean) => void;
  onStatus: (message: string | null) => void;
}

const sameSet = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((x) => b.includes(x));

/**
 * Part 5: a card per project the team gives access to, each with its own Save, as the member page's cards
 * are but without "Narrow on a database" (teams carry no overrides, R7).
 */
export function TeamAccess({ team, me, canEdit, onDirtyChange, onStatus }: TeamAccessProps) {
  const visible = me.tenant?.projects ?? [];
  const [added, setAdded] = useState<string[]>([]);
  const held = new Set(team.projects.map((p) => p.project_id));
  const cards: { project: Project; access: TeamProjectAccess | null }[] = [
    ...team.projects.flatMap((a) => {
      const project = visible.find((p) => p.id === a.project_id);
      return project ? [{ project, access: a }] : [];
    }),
    ...added.flatMap((id) => {
      const project = visible.find((p) => p.id === id);
      return project && !held.has(id) ? [{ project, access: null }] : [];
    }),
  ];
  const candidates = canEdit ? visible.filter((p) => !held.has(p.id) && !added.includes(p.id)) : [];

  return (
    <div className="space-y-4">
      {cards.length === 0 ? (
        <p className="text-sm text-chalk-dim">{teamName(team)} gives no project access yet.</p>
      ) : (
        cards.map(({ project, access }) =>
          canEdit ? (
            <EditableCard
              key={project.id}
              team={team}
              project={project}
              access={access}
              onDirtyChange={onDirtyChange}
              onStatus={onStatus}
              onGone={() => setAdded((a) => a.filter((id) => id !== project.id))}
            />
          ) : (
            <Frame key={project.id} project={project}>
              {access ? <ReadOnly access={access} /> : null}
            </Frame>
          ),
        )
      )}
      {candidates.length > 0 && <AddAccess candidates={candidates} onAdd={(id) => setAdded((a) => [...a, id])} />}
    </div>
  );
}

function Frame({ project, dirty, children }: { project: Project; dirty?: boolean; children: ReactNode }) {
  const id = useId();
  return (
    <section aria-labelledby={id} className={cn("border bg-ink", dirty ? "border-line-strong" : "border-line")}>
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1 border-b border-line px-4 py-3">
        <h3 id={id} className="text-sm font-semibold">
          {project.name}
        </h3>
        {project.databases.length > 0 && <span className="text-xs text-chalk-dim">{project.databases.map((d) => d.name).join(", ")}</span>}
        {dirty && (
          <span className="ml-auto inline-flex items-center gap-2 text-xs text-chalk">
            <span aria-hidden="true" className="inline-block size-2 bg-sand" />
            Unsaved changes
          </span>
        )}
      </div>
      <div className="p-4">{children}</div>
    </section>
  );
}

/** What the caller may see of a team's grant: `pg_roles: null` means the platform role only (P11, MH-34). */
function ReadOnly({ access }: { access: TeamProjectAccess }) {
  return (
    <div className="space-y-2 text-sm">
      <p>
        <span className="font-medium">Platform role</span> <span>{ROLE_LABEL[access.platform_role]}</span>
      </p>
      {access.pg_roles !== null && (
        <p>
          <span className="font-medium">Postgres roles</span>{" "}
          {access.platform_role === "admin" ? (
            "All roles"
          ) : access.pg_roles.length === 0 ? (
            "None"
          ) : (
            access.pg_roles.map((r, i) => (
              <span key={r}>
                {i > 0 && ", "}
                <span className="font-mono text-[13px]">{escapeName(r)}</span>
              </span>
            ))
          )}
        </p>
      )}
    </div>
  );
}

interface EditableProps {
  team: Team;
  project: Project;
  /** The team's grant, or null for a card being added. */
  access: TeamProjectAccess | null;
  onDirtyChange: (projectId: string, dirty: boolean) => void;
  onStatus: (message: string | null) => void;
  onGone: () => void;
}

function EditableCard({ team, project, access, onDirtyChange, onStatus, onGone }: EditableProps) {
  const queryClient = useQueryClient();
  const { sources, loading } = useRoleSources(project.databases);
  const creating = access === null;
  const saved: string[] = access?.pg_roles ?? [];
  const [platformRole, setPlatformRole] = useState<PlatformRole>(access?.platform_role ?? "viewer");
  const [pgRoles, setPgRoles] = useState<string[]>(saved);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<Failure | null>(null);
  const [removing, setRemoving] = useState(false);
  const [removeError, setRemoveError] = useState<string | null>(null);

  // A save refreshes the team: the card starts again from what the server stored.
  const storedKey = JSON.stringify(access);
  useEffect(() => {
    setPlatformRole(access?.platform_role ?? "viewer");
    setPgRoles(access?.pg_roles ?? []);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [storedKey]);

  const admin = platformRole === "admin";
  const choices = roleChoices(sources, platformRole);
  const adminOnly = admin ? [] : adminOnlyChecked(choices, pgRoles);
  const notAllowed = adminOnly.length > 0;
  const dirty = access !== null && (platformRole !== access.platform_role || (!admin && !sameSet(pgRoles, saved)));
  const changed = dirty || creating;
  const report = useCallback((d: boolean) => onDirtyChange(project.id, d), [onDirtyChange, project.id]);
  useEffect(() => report(changed), [changed, report]);
  useEffect(() => () => report(false), [report]);

  async function save() {
    if (busy || !changed || notAllowed) return;
    setBusy(true);
    setFailure(null);
    try {
      await putTeamGrant(team.id, project.id, { platform_role: platformRole, pg_roles: admin ? [] : pgRoles });
      refreshTeams(queryClient);
      onStatus(`${teamName(team)} now gives ${ROLE_LABEL[platformRole]} on ${project.name}.`);
      if (creating) onGone();
    } catch (err) {
      setFailure(failureOf(err, project, { teamName: (id) => (id === team.id ? teamName(team) : undefined) }));
    } finally {
      setBusy(false);
    }
  }

  async function takeAway() {
    setBusy(true);
    setRemoveError(null);
    try {
      await deleteTeamGrant(team.id, project.id);
      refreshTeams(queryClient);
      setRemoving(false);
      onStatus(`${teamName(team)} no longer gives access to ${project.name}.`);
    } catch (err) {
      setRemoveError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Frame project={project} dirty={dirty}>
      {loading ? (
        <p className="text-sm text-chalk-dim">Loading roles…</p>
      ) : (
        <form
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            void save();
          }}
          className="space-y-4"
        >
          <fieldset disabled={busy} className="m-0 min-w-0 space-y-4 border-0 p-0">
            <PlatformRoleField
              value={platformRole}
              onChange={(role) => setPlatformRole(role)}
              label={`Platform role on ${project.name}`}
            />
            {admin ? (
              <div className="space-y-1">
                <p className="text-sm font-medium">Postgres roles</p>
                <p className="text-sm">All roles</p>
                <p className="text-xs text-chalk-dim">An Admin holds every role of the project, the owner included.</p>
              </div>
            ) : (
              <div className="space-y-1">
                <p className="text-sm font-medium">Postgres roles</p>
                {choices.length === 0 ? (
                  <p className="text-sm text-chalk-dim">No roles to grant yet.</p>
                ) : (
                  <RoleChecklist choices={choices} checked={pgRoles} baseline={saved} invalid={adminOnly} onChange={setPgRoles} />
                )}
              </div>
            )}
          </fieldset>
          {notAllowed && <AdminOnlyNote names={adminOnly} />}
          {failure && <FailureText failure={failure} />}
          <div className="flex flex-wrap items-center justify-between gap-3 border-t border-line bg-deep px-4 py-3 text-sm -mx-4 -mb-4">
            <p className="min-w-0 text-chalk-dim">
              {admin ? "Admin · every role" : `${ROLE_LABEL[platformRole]}${pgRoles.length > 0 ? ` · ${pgRoles.map(escapeName).join(", ")}` : ""}`}
            </p>
            <div className="flex flex-wrap items-center gap-2">
              {!creating && (
                <Button type="button" variant="ghost" size="sm" className="text-coral" disabled={busy} onClick={() => setRemoving(true)}>
                  Take access away
                </Button>
              )}
              {/* Cancel is there whenever the draft differs from what is saved, an error state included. */}
              {changed && (
                <Button
                  type="button"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => {
                    if (creating) {
                      onGone();
                      return;
                    }
                    setPlatformRole(access?.platform_role ?? "viewer");
                    setPgRoles(saved);
                    setFailure(null);
                  }}
                >
                  Cancel
                </Button>
              )}
              {changed && (
                <Button type="submit" disabled={busy || notAllowed}>
                  {busy ? "Saving…" : "Save"}
                </Button>
              )}
            </div>
          </div>
        </form>
      )}
      {removing && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setRemoving(false)}
          title={`Take ${project.name} away from ${teamName(team)}?`}
          body={
            <p>
              {team.member_count === 1 ? "The 1 member" : `The ${team.member_count} members`} of this team lose the access it gives on {project.name}. Access they hold on their own stays.
            </p>
          }
          confirm="Take access away"
          tone="destructive"
          busy={busy}
          error={removeError}
          onConfirm={() => void takeAway()}
        />
      )}
    </Frame>
  );
}

function AddAccess({ candidates, onAdd }: { candidates: Project[]; onAdd: (projectId: string) => void }) {
  const id = useId();
  const [pick, setPick] = useState("");
  return (
    <div className="border border-dashed border-line-strong p-3">
      <div className="flex flex-wrap items-center gap-3">
        <h3 className="text-sm font-semibold">Add access to a project</h3>
        <div>
          <label htmlFor={id} className="sr-only">
            Project to add
          </label>
          <Select id={id} value={pick} onChange={(e) => setPick(e.target.value)} wrapperClassName="min-w-48">
            <option value="">Choose a project</option>
            {candidates.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </Select>
        </div>
        <Button
          variant="outline"
          disabled={pick === ""}
          onClick={() => {
            onAdd(pick);
            setPick("");
          }}
        >
          Add
        </Button>
      </div>
    </div>
  );
}
