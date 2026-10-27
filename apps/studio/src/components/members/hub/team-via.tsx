import { Link } from "@tanstack/react-router";
import { escapeName } from "./escape-name";

/** A team as the markers name it. */
export interface TeamRef {
  id: string;
  name: string;
}

/** A team's name as a link to its page. */
export function TeamLink({ team }: { team: TeamRef }) {
  return (
    <Link to="/app/members/teams/$teamId" params={{ teamId: team.id }} className="text-chalk underline decoration-line-strong underline-offset-4 hover:decoration-chalk">
      {escapeName(team.name)}
    </Link>
  );
}

/** "A", "A and B", "A, B and C" as items (strings or nodes) with the separators between them. */
export function joinAnd<T>(items: readonly T[], render: (item: T) => React.ReactNode, keyOf: (item: T) => string) {
  return items.map((item, i) => (
    <span key={keyOf(item)}>
      {i > 0 && (i === items.length - 1 ? " and " : ", ")}
      {render(item)}
    </span>
  ));
}

/** "the Billing team", "the Billing and Ledger auditors teams": each name a link to its team. */
export function TeamsPhrase({ teams }: { teams: readonly TeamRef[] }) {
  return (
    <>
      the {joinAnd(teams, (t) => <TeamLink team={t} />, (t) => t.id)} {teams.length === 1 ? "team" : "teams"}
    </>
  );
}
