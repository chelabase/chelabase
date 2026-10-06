import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { ProjectPage } from "@/components/project-page";
import { useAppSession } from "@/lib/app-session";

export const Route = createFileRoute("/app/projects/$projectId")({
  validateSearch: (search: Record<string, unknown>): { tab?: "settings" | "users" } =>
    search.tab === "settings" || search.tab === "users" ? { tab: search.tab } : {},
  component: ProjectRoute,
});

function ProjectRoute() {
  const { apiUrl, onLoaded } = useAppSession();
  const { projectId } = Route.useParams();
  const { tab } = Route.useSearch();
  const navigate = useNavigate();
  return (
    <ProjectPage
      projectId={projectId}
      apiUrl={apiUrl}
      onLoaded={onLoaded}
      tab={tab ?? "databases"}
      onTabChange={(next) =>
        void navigate({
          to: "/app/projects/$projectId",
          params: { projectId },
          search: next === "databases" ? {} : { tab: next },
          replace: true,
        })
      }
      onGone={() => void navigate({ to: "/app", replace: true })}
    />
  );
}
