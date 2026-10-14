import { useState, type RefObject } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  deleteProjectGrant,
  getListProjectGrantsQueryKey,
  putProjectGrant,
  type PlatformRole,
  type Project,
  type ProjectGrant,
} from "@chelabase/management-client";
import { refreshAccess } from "@/components/members/keys";
import { PlatformRoleField } from "@/components/members/platform-role-field";
import { RolePicker } from "@/components/members/role-picker";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { failureOf, FailureText, type Failure } from "./failure";
import { DatabaseNarrowing } from "./database-narrowing";

export interface GrantEditorProps {
  project: Project;
  grant: ProjectGrant;
  name: string;
  chains: ReadonlyMap<string, string[]>;
  /** Closes the editor; `done` names what happened, `gone` says the row no longer exists. */
  onClose: (done?: string, gone?: boolean) => void;
  onStatus: (message: string) => void;
  /** Where focus goes when the dialog closes and the row is gone. */
  returnFocus: RefObject<HTMLElement | null>;
}

/** The editor's element id, which the row's expand control points at. */
export const editorId = (memberId: string) => `access-editor-${memberId}`;

const sameSet = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((x) => b.includes(x));

/** An expanded row: the grant (platform role, roles, save, take away) and the per-database narrowing. */
export function GrantEditor({ project, grant, name, chains, onClose, onStatus, returnFocus }: GrantEditorProps) {
  const queryClient = useQueryClient();
  const [platformRole, setPlatformRole] = useState<PlatformRole>(grant.platform_role);
  // R17: an Admin grant holds every role (`"all"`); a grant moved below Admin starts with none.
  const granted = grant.pg_roles === "all" ? [] : grant.pg_roles;
  const [pgRoles, setPgRoles] = useState<string[]>(granted);
  const [blocked, setBlocked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<Failure | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [confirmError, setConfirmError] = useState<string | null>(null);

  const admin = platformRole === "admin";
  const changed = platformRole !== grant.platform_role || (!admin && !sameSet(pgRoles, granted));
  // The role picker isn't shown for Admin, so nothing it held back counts.
  const held = blocked && !admin;
  const refresh = () => {
    void queryClient.invalidateQueries({ queryKey: getListProjectGrantsQueryKey(project.id) });
    refreshAccess(queryClient);
  };

  async function save() {
    if (busy || !changed || held) return;
    setBusy(true);
    setFailure(null);
    try {
      await putProjectGrant(project.id, grant.member_id, { platform_role: platformRole, pg_roles: admin ? [] : pgRoles });
      refresh();
      onClose(`Saved ${name}'s access.`);
    } catch (err) {
      setFailure(failureOf(err, project));
      refresh();
    } finally {
      setBusy(false);
    }
  }

  async function takeAway() {
    setBusy(true);
    setConfirmError(null);
    try {
      await deleteProjectGrant(project.id, grant.member_id);
      refresh();
      setConfirming(false);
      onClose(`Took ${name}'s access to ${project.name} away.`, true);
    } catch (err) {
      setConfirmError(failureOf(err, project).message);
      refresh();
    } finally {
      setBusy(false);
    }
  }

  return (
    <div id={editorId(grant.member_id)} role="group" aria-label={`Access for ${name}`} className="space-y-4 border-l-2 border-line-strong py-3 pl-4">
      <form
        aria-label={`Project access for ${name}`}
        onSubmit={(e) => {
          e.preventDefault();
          void save();
        }}
        className="space-y-4"
      >
        <fieldset disabled={busy} className="m-0 min-w-0 space-y-4 border-0 p-0">
          <PlatformRoleField value={platformRole} onChange={setPlatformRole} />
          <RolePicker
            databases={project.databases}
            platformRole={platformRole}
            value={pgRoles}
            onChange={setPgRoles}
            refused={failure?.role ? new Set([failure.role]) : undefined}
            onBlockedChange={setBlocked}
          />
        </fieldset>
        {failure && <FailureText failure={failure} />}
        <div className="flex flex-wrap items-center gap-2">
          <Button type="submit" disabled={busy || !changed || held}>
            {busy ? "Saving…" : "Save"}
          </Button>
          <Button type="button" variant="ghost" disabled={busy} onClick={() => onClose()}>
            Cancel
          </Button>
          <Button
            type="button"
            variant="destructive"
            className="ml-auto"
            disabled={busy}
            onClick={() => {
              setConfirmError(null);
              setConfirming(true);
            }}
          >
            Take access away
          </Button>
        </div>
      </form>
      <div className="border-t border-line pt-3">
        <h4 className="text-sm font-medium">Per database</h4>
        <p className="text-xs text-chalk-dim">Narrowing only takes access away on one database; it never adds any.</p>
        <div className="divide-y divide-line">
          {project.databases.map((d) => (
            <DatabaseNarrowing
              key={d.database_id}
              project={project}
              database={d}
              memberId={grant.member_id}
              memberName={name}
              grant={{ platformRole: grant.platform_role, pgRoles: grant.pg_roles }}
              override={grant.overrides.find((o) => o.database_id === d.database_id)}
              chains={chains}
              // Only a narrowing can be "what's left of the grant": with none, the grant itself is refused.
              flagged={failure?.databaseId === d.database_id && grant.overrides.some((o) => o.database_id === d.database_id)}
              onStatus={onStatus}
            />
          ))}
        </div>
      </div>
      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title={`Take ${name}'s access to ${project.name} away?`}
        body="They can't open this project or act as its roles any more; their open sessions as those roles end. Per-database narrowings are kept and apply if they get access again."
        confirm="Take access away"
        tone="destructive"
        busy={busy}
        finalFocus={returnFocus}
        error={confirmError}
        onConfirm={() => void takeAway()}
      />
    </div>
  );
}
