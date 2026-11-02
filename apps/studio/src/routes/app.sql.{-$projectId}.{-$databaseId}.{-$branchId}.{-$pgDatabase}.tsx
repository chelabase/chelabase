import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { SqlPage } from "@/components/sql-editor/sql-page";
import { editorRouteParams, sqlRouteParams, type EditorParams } from "@/lib/table-editor/context";

export const Route = createFileRoute("/app/sql/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}")({
  component: SqlRoute,
});

function SqlRoute() {
  const params: EditorParams = Route.useParams();
  const navigate = useNavigate();
  return (
    <SqlPage
      params={params}
      navigate={(p, opts) =>
        void navigate({
          to: "/app/sql/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}",
          // Every level named, so the ones below a picked level are cleared.
          params: sqlRouteParams(p),
          replace: opts?.replace,
        })
      }
      openTables={(p) =>
        void navigate({
          to: "/app/tables/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}/{-$schema}/{-$table}",
          params: editorRouteParams(p),
          search: { sql: 1 },
        })
      }
    />
  );
}
