import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { RouteDraftBlocker } from "@/components/table-editor/route-blocker";
import { TableEditor } from "@/components/table-editor/table-editor";
import { editorRouteParams, sqlRouteParams, type EditorParams } from "@/lib/table-editor/context";

/** `?sql=1`: the SQL drawer is open under the table (the SQL tab's Quick view links to it). */
const validateTablesSearch = (s: Record<string, unknown>): { sql?: 1 } => (s.sql === 1 || s.sql === "1" ? { sql: 1 } : {});

export const Route = createFileRoute("/app/tables/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}/{-$schema}/{-$table}")({
  validateSearch: validateTablesSearch,
  component: TablesRoute,
});

function TablesRoute() {
  const params: EditorParams = Route.useParams();
  const { sql } = Route.useSearch();
  const navigate = useNavigate();
  return (
    <TableEditor
      params={params}
      sql={sql === 1}
      leaveGuard={RouteDraftBlocker}
      navigate={(p, opts) =>
        void navigate({
          to: "/app/tables/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}/{-$schema}/{-$table}",
          // Every level named, so the ones below a picked level are cleared (I2).
          params: editorRouteParams(p),
          // The drawer stays as it is unless this opens or closes it.
          search: (prev) => (opts?.sql === undefined ? prev : opts.sql ? { ...prev, sql: 1 as const } : { ...prev, sql: undefined }),
          replace: opts?.replace,
          // Opening or closing the drawer leaves the table: the row draft blocker has nothing to ask.
          ignoreBlocker: opts?.sql !== undefined,
        })
      }
      openSqlEditor={(p) => void navigate({ to: "/app/sql/{-$projectId}/{-$databaseId}/{-$branchId}/{-$pgDatabase}", params: sqlRouteParams(p) })}
    />
  );
}
