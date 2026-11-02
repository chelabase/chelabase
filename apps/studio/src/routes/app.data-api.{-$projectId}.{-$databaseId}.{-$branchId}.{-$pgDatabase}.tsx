import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { DataApiPage } from "@/components/data-api-page";
import { useAppSession } from "@/lib/app-session";
import { sqlRouteParams, type EditorParams } from "@/lib/table-editor/context";

export const Route = createFileRoute("/app/data-api/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}")({
  component: DataApiRoute,
});

function DataApiRoute() {
  const params: EditorParams = Route.useParams();
  const navigate = useNavigate();
  const { apiUrl } = useAppSession();
  return (
    <DataApiPage
      params={params}
      apiUrl={apiUrl}
      navigate={(p, opts) =>
        void navigate({
          to: "/app/data-api/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}",
          // Every level named, so the ones below a picked level are cleared.
          params: sqlRouteParams(p),
          replace: opts?.replace,
        })
      }
    />
  );
}
