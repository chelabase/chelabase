import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { FunctionsPage, type FunctionsParams } from "@/components/functions/functions-page";
import { useAppSession } from "@/lib/app-session";

export const Route = createFileRoute("/app/functions/{-$projectId}/{-$databaseId}/{-$branchId}")({
  component: FunctionsRoute,
});

function FunctionsRoute() {
  const params: FunctionsParams = Route.useParams();
  const navigate = useNavigate();
  const { apiUrl } = useAppSession();
  return (
    <FunctionsPage
      params={params}
      apiUrl={apiUrl}
      navigate={(p, opts) =>
        void navigate({
          to: "/app/functions/{-$projectId}/{-$databaseId}/{-$branchId}",
          // Every level named, so the ones below a picked level are cleared.
          params: { projectId: p.projectId || undefined, databaseId: p.databaseId || undefined, branchId: p.branchId || undefined },
          replace: opts?.replace,
        })
      }
    />
  );
}
