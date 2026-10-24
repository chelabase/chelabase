import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { MembersPage } from "@/components/members/members-page";
import { useAppSession } from "@/lib/app-session";
import { validateMembersSearch } from "@/lib/members-search";

export const Route = createFileRoute("/app/members")({
  validateSearch: validateMembersSearch,
  component: MembersRoute,
});

function MembersRoute() {
  const { onSignOut } = useAppSession();
  const queryClient = useQueryClient();
  const { tab } = Route.useSearch();
  const navigate = useNavigate();
  return (
    <MembersPage
      tab={tab ?? "people"}
      onTabChange={(next) => void navigate({ to: "/app/members", search: next === "people" ? {} : { tab: next }, replace: true })}
      onSignOut={() => {
        queryClient.clear();
        onSignOut();
      }}
    />
  );
}
