import { useId, useMemo, useState, type ReactNode } from "react";
import { Link, useNavigate, useRouter, useSearch } from "@tanstack/react-router";
import { Plus, Search } from "lucide-react";
import { useListTeams, type Project, type Team } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { errorText } from "@/lib/errors";
import type { HubSearch } from "@/lib/members-search";
import { useViewPreference, ViewToggle, type RosterView } from "../hub";
import { byName, membersText, projectsText, teamName } from "./team-model";
import { TeamDialog } from "./team-dialog";

export interface TeamsTabProps {
  tenantAdmin: boolean;
  /** The caller's visible projects, for naming a team's projects. */
  projects: readonly Project[];
  onStatus?: (message: string | null) => void;
}

interface Nav {
  view: RosterView | undefined;
  q: string;
  setView: (view: RosterView) => void;
  setQ: (q: string) => void;
  teamLink: (t: Team, className: string, children: ReactNode) => ReactNode;
  open: (t: Team) => void;
}

const LINK = "underline-offset-4 outline-none hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sand";

/** The Teams tab. Inside the router its view and filter live in the URL (the team page's back link restores them). */
export function TeamsTab(props: TeamsTabProps) {
  const router = useRouter({ warn: false });
  return router ? <RoutedTeamsTab {...props} /> : <LocalTeamsTab {...props} />;
}

function RoutedTeamsTab(props: TeamsTabProps) {
  const search = useSearch({ strict: false }) as HubSearch;
  const navigate = useNavigate({ from: "/app/members" });
  const carry = { ...(search.view && { view: search.view }), ...(search.q && { q: search.q }) };
  const nav: Nav = {
    view: search.view,
    q: search.q ?? "",
    setView: (view) => void navigate({ search: (prev: Record<string, unknown>) => ({ ...prev, view }), replace: true }),
    setQ: (q) => void navigate({ search: (prev: Record<string, unknown>) => ({ ...prev, q: q === "" ? undefined : q }), replace: true }),
    teamLink: (t, className, children) => (
      <Link to="/app/members/teams/$teamId" params={{ teamId: t.id }} search={carry} className={className}>
        {children}
      </Link>
    ),
    open: (t) => void navigate({ to: "/app/members/teams/$teamId", params: { teamId: t.id }, search: carry }),
  };
  return <TeamsList {...props} nav={nav} />;
}

function LocalTeamsTab(props: TeamsTabProps) {
  const [view, setView] = useState<RosterView | undefined>();
  const [q, setQ] = useState("");
  const nav: Nav = {
    view,
    q,
    setView,
    setQ,
    teamLink: (t, className, children) => (
      <a href={`/app/members/teams/${encodeURIComponent(t.id)}`} className={className}>
        {children}
      </a>
    ),
    open: () => {},
  };
  return <TeamsList {...props} nav={nav} />;
}

function TeamsList({ tenantAdmin, projects, onStatus, nav }: TeamsTabProps & { nav: Nav }) {
  const teams = useListTeams();
  const [storedView, storeView] = useViewPreference("teams");
  const view = nav.view ?? storedView;
  const filterId = useId();
  const [query, setQuery] = useState(nav.q);
  const [creating, setCreating] = useState(false);

  const all = useMemo(() => byName(teams.data ?? []), [teams.data]);
  const shown = useMemo(() => {
    const q = query.trim().toLowerCase();
    return q === "" ? all : all.filter((t) => t.name.toLowerCase().includes(q) || t.description.toLowerCase().includes(q));
  }, [all, query]);
  const projectName = (id: string) => projects.find((p) => p.id === id)?.name ?? "a project";
  const projectLine = (t: Team) => (t.projects.length === 0 ? "No projects" : t.projects.map((p) => projectName(p.project_id)).join(", "));

  if (teams.data === undefined) {
    return teams.error ? (
      <div className="space-y-3">
        <p role="alert" className="text-sm text-coral">
          Couldn't load teams: {errorText(teams.error)}
        </p>
        <Button variant="outline" onClick={() => void teams.refetch()}>
          Retry
        </Button>
      </div>
    ) : (
      <p className="text-sm text-chalk-dim">Loading teams…</p>
    );
  }

  return (
    <div className="space-y-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-sm text-chalk-dim tabular-nums">{`${all.length} ${all.length === 1 ? "team" : "teams"}`}</p>
        <div className="flex flex-wrap items-center gap-3">
          <label htmlFor={filterId} className="sr-only">
            Filter teams
          </label>
          <div className="relative">
            <Search aria-hidden="true" size={14} strokeWidth={1.5} className="pointer-events-none absolute top-1/2 left-2 -translate-y-1/2 text-chalk-dim" />
            <input
              id={filterId}
              type="search"
              value={query}
              placeholder="Filter teams"
              onChange={(e) => {
                setQuery(e.target.value);
                nav.setQ(e.target.value);
              }}
              className="h-8 w-56 border border-input bg-ink pr-2 pl-7 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
            />
          </div>
          <ViewToggle
            value={view}
            onChange={(next) => {
              storeView(next);
              nav.setView(next);
            }}
          />
          {tenantAdmin && (
            <Button onClick={() => setCreating(true)}>
              <Plus aria-hidden="true" size={14} strokeWidth={1.5} />
              Create team
            </Button>
          )}
        </div>
      </div>
      {!tenantAdmin && <p className="text-sm text-chalk-dim">Tenant admins create teams and change who is in them. Projects count only the ones you can see.</p>}

      {all.length === 0 ? (
        <p className="max-w-[65ch] text-sm text-chalk-dim">
          {tenantAdmin ? "No teams yet. A team gives the same project access to everyone in it; change the team and everyone in it changes with it." : "No teams yet."}
        </p>
      ) : shown.length === 0 ? (
        <p className="text-sm text-chalk-dim">
          No team matches “{query.trim()}”.{" "}
          <button
            type="button"
            className="text-chalk underline underline-offset-4"
            onClick={() => {
              setQuery("");
              nav.setQ("");
            }}
          >
            Clear filter
          </button>
        </p>
      ) : view === "cards" ? (
        <ul className="grid gap-3 sm:grid-cols-2 md:grid-cols-3">
          {shown.map((t) => (
            <li key={t.id} className="border border-line hover:bg-chalk/[0.06]">
              {nav.teamLink(
                t,
                "block space-y-2 p-3 text-sm outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand",
                <>
                  <span className="block truncate font-semibold text-chalk">{teamName(t)}</span>
                  {t.description && <span className="line-clamp-2 block text-chalk-dim [overflow-wrap:anywhere]">{t.description}</span>}
                  <span className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-chalk-dim">
                    <span>{membersText(t.member_count)}</span>
                    <span>{t.projects.length === 0 ? "No projects" : projectsText(t.projects.length)}</span>
                  </span>
                </>,
              )}
            </li>
          ))}
        </ul>
      ) : (
        <div className="overflow-x-auto">
          <table aria-label="Teams" className="w-full text-left text-sm">
            <thead className="text-xs text-chalk-dim">
              <tr className="border-b border-line">
                <th scope="col" className="py-2 pr-3 font-normal">Team</th>
                <th scope="col" className="py-2 pr-3 font-normal">Members</th>
                <th scope="col" className="py-2 font-normal">Projects</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-line">
              {shown.map((t) => (
                <tr key={t.id}>
                  <td className="py-2 pr-3 align-top">
                    <div className="text-chalk">{nav.teamLink(t, LINK, teamName(t))}</div>
                    {t.description && <div className="line-clamp-1 text-chalk-dim [overflow-wrap:anywhere]">{t.description}</div>}
                  </td>
                  <td className="py-2 pr-3 align-top tabular-nums">{t.member_count}</td>
                  <td className="py-2 align-top text-chalk-dim">
                    <span className="sr-only">{projectsText(t.projects.length)}: </span>
                    {projectLine(t)}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {creating && (
        <TeamDialog
          open
          onOpenChange={(next) => !next && setCreating(false)}
          onSaved={(team) => {
            onStatus?.(`Created the team ${teamName(team)}.`);
            nav.open(team);
          }}
        />
      )}
    </div>
  );
}
