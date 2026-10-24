import { createFileRoute } from "@tanstack/react-router";
import { MemberPage } from "@/components/members/member-page";
import { validateMemberPageSearch } from "@/lib/members-search";

// `members_`: a page of its own in the app shell, not nested in the Members page.
export const Route = createFileRoute("/app/members_/$memberId")({
  validateSearch: validateMemberPageSearch,
  component: MemberRoute,
});

/** A `$memberId` that isn't a UUID is the not-found state (the page handles it and asks for nothing). */
function MemberRoute() {
  const { memberId } = Route.useParams();
  return <MemberPage memberId={memberId} />;
}
