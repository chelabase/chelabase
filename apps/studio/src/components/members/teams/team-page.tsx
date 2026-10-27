import { useCallback, useState, type ReactNode } from "react";
import { Link, useBlocker, useNavigate, useSearch } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { ArrowLeft } from "lucide-react";
import { ApiError, deleteTeam, useGetMe, useGetTeam, useListMembers, type MeResponse, type Member, type Team } from "@chelabase/management-client";
import { useCrumbLabel } from "@/components/app-shell/crumbs";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Modal } from "@/components/ui/dialog";
import { errorText } from "@/lib/errors";
import { isUuid } from "@/lib/members-search";
import { utc } from "@/lib/members";
import { refreshTeams } from "./keys";
import { TeamAccess } from "./team-access";
import { TeamDialog } from "./team-dialog";
import { TeamMembers } from "./team-members";
import { deleteConsequence, membersText, projectsText, teamName } from "./team-model";

export interface TeamPageProps {
  teamId: string;
}

/** The page's column and the way back to the Teams tab, which keeps its view and filter. */
function Shell({ children }: { children: ReactNode }) {
  const search = useSearch({ strict: false }) as { view?: "cards" | "list"; q?: string };
  return (
    <div className="mx-auto max-w-[56rem] space-y-8 p-8 max-sm:p-4">
      <Link
        to="/app/members"
        search={{ tab: "teams", ...(search.view && { view: search.view }), ...(search.q && { q: search.q }) }}
        className="inline-flex items-center gap-1 text-sm text-chalk-dim underline-offset-4 hover:text-chalk hover:underline"
      >
        <ArrowLeft aria-hidden="true" size={16} strokeWidth={1.5} />
        Teams
      </Link>
      {children}
    </div>
  );
}

function NotFound() {
  return (
    <div className="space-y-1">
      <h1 className="text-2xl font-semibold">Team not found</h1>
      <p className="text-sm text-chalk-dim">There is no team with that id, or it was deleted.</p>
    </div>
  );
}

/** `/app/members/teams/$teamId`: a team's name, members and project access (Part 5, Part 7). */
export function TeamPage({ teamId }: TeamPageProps) {
  return <Shell>{isUuid(teamId) ? <Loaded teamId={teamId} /> : <NotFound />}</Shell>;
}

function Loaded({ teamId }: { teamId: string }) {
  const me = useGetMe();
  const team = useGetTeam(teamId);
  const members = useListMembers();
  if (team.error instanceof ApiError && team.error.status === 404) return <NotFound />;
  const failed = me.error ?? team.error ?? members.error;
  if (me.data === undefined || team.data === undefined || members.data === undefined) {
    if (failed) {
      return (
        <div className="space-y-3">
          <p role="alert" className="text-sm text-coral">
            Couldn't load this team: {errorText(failed)}
          </p>
          <Button
            variant="outline"
            onClick={() => {
              void me.refetch();
              void team.refetch();
              void members.refetch();
            }}
          >
            Retry
          </Button>
        </div>
      );
    }
    return <p className="text-sm text-chalk-dim">Loading team…</p>;
  }
  return <Page team={team.data} me={me.data} roster={members.data} />;
}

function Page({ team, me, roster }: { team: Team; me: MeResponse; roster: readonly Member[] }) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const search = useSearch({ strict: false }) as { view?: "cards" | "list"; q?: string };
  const canEdit = me.member?.tenant_admin === true;
  const [status, setStatus] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);
  const [membersDirty, setMembersDirty] = useState(false);
  const [dirtyCards, setDirtyCards] = useState<ReadonlySet<string>>(new Set());
  const setCardDirty = useCallback((id: string, dirty: boolean) => {
    setDirtyCards((prev) => {
      if (prev.has(id) === dirty) return prev;
      const next = new Set(prev);
      if (dirty) next.add(id);
      else next.delete(id);
      return next;
    });
  }, []);
  const dirty = membersDirty || dirtyCards.size > 0;
  const [leaving, setLeaving] = useState(false);
  const blocker = useBlocker({ shouldBlockFn: () => !leaving, withResolver: true, disabled: !dirty });
  const name = teamName(team);
  useCrumbLabel("teamId", team.id, name);

  async function confirmDelete() {
    setDeleteBusy(true);
    setDeleteError(null);
    try {
      await deleteTeam(team.id);
      refreshTeams(queryClient);
      // The team is gone: leaving needn't ask about edits to it.
      setLeaving(true);
      setDeleting(false);
      void navigate({ to: "/app/members", search: { tab: "teams", ...(search.view && { view: search.view }), ...(search.q && { q: search.q }) } });
    } catch (err) {
      setDeleteError(err instanceof ApiError && err.status === 404 ? "This team was already deleted." : errorText(err));
    } finally {
      setDeleteBusy(false);
    }
  }

  return (
    <>
      <header className="space-y-2">
        <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
          <h1 className="min-w-0 text-2xl font-semibold break-words">{name}</h1>
          {canEdit && (
            <div className="flex items-center gap-2">
              <Button variant="outline" onClick={() => setEditing(true)}>
                Edit
              </Button>
              <Button variant="outline" className="text-coral" onClick={() => setDeleting(true)}>
                Delete team
              </Button>
            </div>
          )}
        </div>
        {team.description && <p className="max-w-[65ch] text-sm [overflow-wrap:anywhere] text-chalk-dim">{team.description}</p>}
        <p className="text-sm text-chalk-dim tabular-nums" title={`Updated ${utc(team.updated_at)}`}>
          {membersText(team.member_count)} · {team.projects.length === 0 ? "No projects" : projectsText(team.projects.length)}
        </p>
        {status && (
          <p role="status" className="text-sm text-chalk">
            {status}
          </p>
        )}
      </header>

      <section aria-labelledby="team-members" className="space-y-3">
        <div className="space-y-1">
          <h2 id="team-members" className="text-base font-semibold">
            Members
          </h2>
          <p className="max-w-[65ch] text-sm text-chalk-dim">
            {canEdit ? "Everyone checked gets this team's access, on top of what they hold on their own." : "Who gets this team's access."}
          </p>
        </div>
        <TeamMembers team={team} roster={roster} canEdit={canEdit} onDirtyChange={setMembersDirty} />
      </section>

      <section aria-labelledby="team-access" className="space-y-3">
        <div className="space-y-1">
          <h2 id="team-access" className="text-base font-semibold">
            Project access
          </h2>
          <p className="max-w-[65ch] text-sm text-chalk-dim">
            {canEdit
              ? "A platform role and the Postgres roles it may act as, per project. Each card saves on its own and changes everyone in the team."
              : "The platform role this team gives on each project you can see."}
          </p>
        </div>
        <TeamAccess team={team} me={me} canEdit={canEdit} onDirtyChange={setCardDirty} onStatus={setStatus} />
      </section>

      {editing && <TeamDialog open onOpenChange={(next) => !next && setEditing(false)} team={team} onSaved={(t) => setStatus(`Saved ${teamName(t)}.`)} />}
      {deleting && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setDeleting(false)}
          title={`Delete ${name}?`}
          body={<p>{deleteConsequence(team)}</p>}
          confirm="Delete team"
          tone="destructive"
          busy={deleteBusy}
          error={deleteError}
          onConfirm={() => void confirmDelete()}
        />
      )}
      <Modal
        open={blocker.status === "blocked"}
        onOpenChange={(next) => !next && blocker.reset?.()}
        title="Discard unsaved changes?"
        description="This team has changes that aren't saved. Leaving drops them."
      >
        <div className="flex justify-end gap-2">
          <Button variant="ghost" onClick={() => blocker.reset?.()}>
            Keep editing
          </Button>
          <Button variant="destructive" onClick={() => blocker.proceed?.()}>
            Discard
          </Button>
        </div>
      </Modal>
    </>
  );
}
