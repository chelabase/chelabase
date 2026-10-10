import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { MembersPage, parseMembersTab, type MembersTab } from "@/components/members/members-page";
import { useAppSession } from "@/lib/app-session";

export const Route = createFileRoute("/app/members")({
  validateSearch: (search: Record<string, unknown>): { tab?: MembersTab } => {
    const tab = parseMembersTab(search.tab);
    return tab && tab !== "people" ? { tab } : {};
  },
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
