import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { TableEditor } from "@/components/table-editor/table-editor";
import { editorRouteParams, type EditorParams } from "@/lib/table-editor/context";

export const Route = createFileRoute("/app/tables/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}/{-$schema}/{-$table}")({
  component: TablesRoute,
});

function TablesRoute() {
  const params: EditorParams = Route.useParams();
  const navigate = useNavigate();
  return (
    <TableEditor
      params={params}
      navigate={(p, opts) =>
        void navigate({
          to: "/app/tables/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}/{-$schema}/{-$table}",
          // Every level named, so the ones below a picked level are cleared (I2).
          params: editorRouteParams(p),
          replace: opts?.replace,
        })
      }
    />
  );
}
