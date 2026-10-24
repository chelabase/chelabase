import { useEffect, useId, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { deleteProjectGrant, getListProjectGrantsQueryKey, putProjectGrant, type DatabaseSummary, type Member, type PlatformRole, type ProjectGrant } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { failureOf, FailureText, type Failure } from "../../project-access/failure";
import { refreshAccess } from "../keys";
import { PlatformRoleField } from "../platform-role-field";
import { roleChoices, type RoleSource } from "../role-picker";
import { RoleChecklist } from "./role-checklist";

export interface GrantCardProps {
  projectId: string;
  member: Member;
  grant: ProjectGrant;
  roleSources: readonly RoleSource[];
  /** Called once after a successful Save, with the grant the server stored. */
  onSaved: (saved: ProjectGrant) => void;
  /** Told whenever the card gains or loses unsaved edits (a page blocks leaving while any card is dirty). */
  onDirtyChange?: (dirty: boolean) => void;
  /** The member has no grant on this project yet: `grant` is a blank to start from, and Save is open from the start. */
  creating?: boolean;
  /** The project's databases: a refusal names the database it came from. Without them none is named. */
  databases?: DatabaseSummary[];
  /** Told each refusal (and its clearing), so the caller can flag the database line it names. */
  onFailure?: (failure: Failure | null) => void;
  /** Told after "No access" was saved and the grant deleted. */
  onRemoved?: () => void;
  /**
   * The footer's lead line, given what the card would save right now (so it follows unsaved edits).
   * Without it the footer shows only when there is something to save.
   */
  summary?: (draft: { platformRole: PlatformRole; pgRoles: readonly string[] }) => ReactNode;
  /** Beside Discard and Save in the footer (a "Take access away" button). */
  footerAction?: ReactNode;
  /** The footer runs to the edges of a padded parent (the member page's card). */
  bleed?: boolean;
  /** Content between the form and the footer (the member page's "Narrow on a database"). */
  children?: ReactNode;
}

const sameSet = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((x) => b.includes(x));

/**
 * One project's access for one member: a platform role and the Postgres roles, saved together.
 * The PUT carries the member's own grant only (MH-38a): never a team's roles, never an override.
 */
export function GrantCard({ projectId, member, grant, roleSources, onSaved, onDirtyChange, creating = false, databases, onFailure, onRemoved, summary, footerAction, bleed = false, children }: GrantCardProps) {
  const queryClient = useQueryClient();
  // R17: an Admin grant holds every role (`"all"`); a grant moved below Admin starts with none.
  const granted = grant.pg_roles === "all" ? [] : grant.pg_roles;
  const [platformRole, setPlatformRole] = useState<PlatformRole>(grant.platform_role);
  const [pgRoles, setPgRoles] = useState<string[]>(granted);
  const [removing, setRemoving] = useState(false);
  const formId = useId();
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<Failure | null>(null);

  const admin = platformRole === "admin";
  const choices = roleChoices(roleSources, platformRole);
  const notAllowed = !admin && pgRoles.some((r) => choices.find((c) => c.value === r)?.disabledReason);
  const dirty = removing || platformRole !== grant.platform_role || (!admin && !sameSet(pgRoles, granted));
  const changed = dirty || creating;
  useEffect(() => {
    onDirtyChange?.(dirty);
  }, [dirty, onDirtyChange]);
  // A card that goes away (saved and replaced, or taken away) is no longer dirty.
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);

  async function save() {
    if (busy || !changed || notAllowed) return;
    setBusy(true);
    setFailure(null);
    onFailure?.(null);
    try {
      if (removing) {
        await deleteProjectGrant(projectId, member.id);
        void queryClient.invalidateQueries({ queryKey: getListProjectGrantsQueryKey(projectId) });
        refreshAccess(queryClient);
        onRemoved?.();
        return;
      }
      const saved = await putProjectGrant(projectId, member.id, { platform_role: platformRole, pg_roles: admin ? [] : pgRoles });
      void queryClient.invalidateQueries({ queryKey: getListProjectGrantsQueryKey(projectId) });
      refreshAccess(queryClient);
      onSaved(saved);
    } catch (err) {
      const failed = failureOf(err, databases ? { databases } : null);
      setFailure(failed);
      onFailure?.(failed);
    } finally {
      setBusy(false);
    }
  }

  function discard() {
    setRemoving(false);
    setPlatformRole(grant.platform_role);
    setPgRoles(granted);
    setFailure(null);
    onFailure?.(null);
  }

  const lead = summary?.({ platformRole, pgRoles: admin ? [] : pgRoles });
  const showFooter = changed || lead !== undefined || footerAction !== undefined;

  return (
    <div>
    <form
      id={formId}
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
          onChange={(role) => {
            setRemoving(false);
            setPlatformRole(role);
          }}
          noAccess={creating ? undefined : { selected: removing, onSelect: () => setRemoving(true) }}
        />
        {removing ? null : admin ? (
          <div className="space-y-1">
            <p className="text-sm font-medium">Postgres roles</p>
            <p className="text-sm">All roles</p>
            <p className="text-xs text-chalk-dim">An Admin holds every role of the project, the owner included.</p>
          </div>
        ) : (
          <div className="space-y-1">
            <div className="flex flex-wrap items-baseline justify-between gap-x-3">
              <p className="text-sm font-medium">Postgres roles</p>
              {choices.length > 1 && <p className="text-xs text-chalk-dim">Checked roles come first; a role&apos;s chain comes with it</p>}
            </div>
            {choices.length === 0 ? (
              <p className="text-sm text-chalk-dim">No roles to grant yet.</p>
            ) : (
              <RoleChecklist choices={choices} checked={pgRoles} baseline={granted} onChange={setPgRoles} />
            )}
          </div>
        )}
      </fieldset>
      {notAllowed && !removing && (
        <p className="text-xs text-coral">Remove the Admin-only roles, or keep Admin.</p>
      )}
      {failure && <FailureText failure={failure} />}
    </form>
    {children}
    {showFooter && (
      <div
        className={cn(
          "mt-4 flex flex-wrap items-center justify-between gap-3 border-t border-line bg-deep px-4 py-3 text-sm",
          bleed && "-mx-4 -mb-4",
        )}
      >
        {lead !== undefined ? <p className="min-w-0 text-chalk-dim">{lead}</p> : <span />}
        <div className="flex flex-wrap items-center gap-2">
          {footerAction}
          {dirty && (
            <Button type="button" variant="ghost" disabled={busy} onClick={discard}>
              Discard
            </Button>
          )}
          {changed && (
            <Button type="submit" form={formId} disabled={busy || notAllowed}>
              {busy ? "Saving…" : "Save"}
            </Button>
          )}
        </div>
      </div>
    )}
    </div>
  );
}
