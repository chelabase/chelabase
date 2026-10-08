import { createFileRoute } from "@tanstack/react-router";
import { MergePage } from "@/components/merges/merge-page";

// `$projectId_`: a page of its own in the app shell, not nested in the project page.
export const Route = createFileRoute("/app/projects/$projectId_/databases/$databaseId/merges/$mergeId")({
  component: MergeRoute,
});

function MergeRoute() {
  const { projectId, databaseId, mergeId } = Route.useParams();
  return <MergePage projectId={projectId} databaseId={databaseId} mergeId={mergeId} />;
}
