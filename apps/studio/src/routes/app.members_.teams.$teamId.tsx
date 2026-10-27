import { createFileRoute } from "@tanstack/react-router";
import { TeamPage } from "@/components/members/teams";
import { validateTeamPageSearch } from "@/lib/members-search";

// `members_`: a page of its own in the app shell, not nested in the Members page.
export const Route = createFileRoute("/app/members_/teams/$teamId")({
  validateSearch: validateTeamPageSearch,
  component: TeamRoute,
});

/** A `$teamId` that isn't a UUID is the not-found state (the page handles it and asks for nothing). */
function TeamRoute() {
  const { teamId } = Route.useParams();
  return <TeamPage teamId={teamId} />;
}
