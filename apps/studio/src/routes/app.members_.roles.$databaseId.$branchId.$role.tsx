import { createFileRoute } from "@tanstack/react-router";
import { RolePage } from "@/components/members/roles/role-page";
import { validateRolePageSearch } from "@/lib/members-search";

// `members_`: a page of its own in the app shell, not nested in the Members page.
export const Route = createFileRoute("/app/members_/roles/$databaseId/$branchId/$role")({
  validateSearch: validateRolePageSearch,
  component: RoleRoute,
});

/** One role on one branch: its privileges in a Postgres database (`?pg_database=`, else the branch's main one). */
function RoleRoute() {
  const { databaseId, branchId, role } = Route.useParams();
  const { pg_database } = Route.useSearch();
  return <RolePage key={`${databaseId}/${branchId}/${role}/${pg_database ?? ""}`} databaseId={databaseId} branchId={branchId} role={role} pgDatabase={pg_database} />;
}
