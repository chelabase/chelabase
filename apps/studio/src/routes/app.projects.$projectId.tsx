import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { ProjectPage } from "@/components/project-page";
import { useAppSession } from "@/lib/app-session";
import { connectionFocus, parseProjectSearch, type ProjectSearch } from "@/lib/project-search";

export const Route = createFileRoute("/app/projects/$projectId")({
  validateSearch: (search: Record<string, unknown>): ProjectSearch => parseProjectSearch(search),
  component: ProjectRoute,
});

function ProjectRoute() {
  const { apiUrl, onLoaded } = useAppSession();
  const { projectId } = Route.useParams();
  const search = Route.useSearch();
  const { tab } = search;
  const navigate = useNavigate();
  return (
    <ProjectPage
      projectId={projectId}
      apiUrl={apiUrl}
      onLoaded={onLoaded}
      focus={connectionFocus(search)}
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
