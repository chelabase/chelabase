import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { StoragePage, type StorageParams } from "@/components/storage/storage-page";
import { useStudioEnv } from "@/lib/env-context";
import { sqlRouteParams } from "@/lib/table-editor/context";

export const Route = createFileRoute("/app/storage/{-$projectId}/{-$databaseId}/{-$branchId}/{-$bucket}/$")({
  component: StorageRoute,
});

function StorageRoute() {
  const { _splat, ...levels } = Route.useParams();
  const params: StorageParams = { ...levels, path: _splat || undefined };
  const navigate = useNavigate();
  const { apiUrl } = useStudioEnv();
  return (
    <StoragePage
      params={params}
      apiUrl={apiUrl}
      navigate={(p, opts) =>
        void navigate({
          to: "/app/storage/{-$projectId}/{-$databaseId}/{-$branchId}/{-$bucket}/$",
          // Every level named, so the ones below a picked level are cleared.
          params: {
            projectId: p.projectId || undefined,
            databaseId: p.databaseId || undefined,
            branchId: p.branchId || undefined,
            bucket: p.bucket || undefined,
            _splat: p.path || undefined,
          },
          replace: opts?.replace,
        })
      }
      openDataApi={(p) =>
        void navigate({
          to: "/app/data-api/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}",
          params: sqlRouteParams(p),
        })
      }
    />
  );
}
