import { useState, type RefObject } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { deleteProjectGrant, getListProjectGrantsQueryKey, type Member, type Project, type ProjectGrant } from "@chelabase/management-client";
import { GrantCard } from "@/components/members/hub";
import { refreshAccess } from "@/components/members/keys";
import type { RoleSource } from "@/components/members/role-picker";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { failureOf, type Failure } from "./failure";
import { DatabaseNarrowing } from "./database-narrowing";

export interface GrantEditorProps {
  project: Project;
  grant: ProjectGrant;
  /** The person; a former member is gone from the list, so only the grant's id is known. */
  member: Member | undefined;
  name: string;
  roleSources: readonly RoleSource[];
  /** The databases' roles are still being read: the card waits for them. */
  rolesLoading: boolean;
  chains: ReadonlyMap<string, string[]>;
  /** Closes the editor; `done` names what happened, `gone` says the row no longer exists. */
  onClose: (done?: string, gone?: boolean) => void;
  onStatus: (message: string) => void;
  /** Where focus goes when the dialog closes and the row is gone. */
  returnFocus: RefObject<HTMLElement | null>;
}

/** The editor's element id, which the row's expand control points at. */
export const editorId = (memberId: string) => `access-editor-${memberId}`;

/** An expanded row: the shared grant card (platform role, roles, save), take away, and the per-database narrowing. */
export function GrantEditor({ project, grant, member, name, roleSources, rolesLoading, chains, onClose, onStatus, returnFocus }: GrantEditorProps) {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [confirmError, setConfirmError] = useState<string | null>(null);
  const [refused, setRefused] = useState<Failure | null>(null);

  const refresh = () => {
    void queryClient.invalidateQueries({ queryKey: getListProjectGrantsQueryKey(project.id) });
    refreshAccess(queryClient);
  };

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
      {rolesLoading ? (
        <p className="text-sm text-chalk-dim">Loading roles…</p>
      ) : (
        <GrantCard
          projectId={project.id}
          // GrantCard reads only the member's id.
          member={member ?? ({ id: grant.member_id } as Member)}
          grant={grant}
          roleSources={roleSources}
          databases={project.databases}
          onFailure={setRefused}
          onSaved={() => onClose(`Saved ${name}'s access.`)}
        />
      )}
      <div className="flex flex-wrap items-center gap-2">
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
              flagged={refused?.databaseId === d.database_id && grant.overrides.some((o) => o.database_id === d.database_id)}
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
