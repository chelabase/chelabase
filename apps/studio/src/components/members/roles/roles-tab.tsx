import { Select } from "@/components/ui/select";
import { useEffect, useId, useMemo, useState, type ReactNode } from "react";
import { Link, useLocation, useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { Lock, Plus, Search } from "lucide-react";
import {
  getListRolesQueryKey,
  useGetMe,
  useListBranchDatabases,
  useListBranches,
  useListDatabasePersonalLogins,
  useListMembers,
  useListProjectGrants,
  useListRoles,
  type Branch,
  type BranchRole,
  type Member,
  type PersonalLoginView,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { projectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { memberName } from "@/lib/members";
import { validateMembersSearch, type MembersSearch } from "@/lib/members-search";
import { cn } from "@/lib/utils";
import { escapeName, useViewPreference, ViewToggle, type RosterView } from "../hub";
import { daysFrom, phaseOf, plural } from "../member-page/logins";
import { Tag } from "./bits";
import { CreateRoleDialog } from "./create-role-dialog";
import { holdersOf, managedRoles, membersText } from "./model";
import { usePrivilegeSummary } from "./use-privilege-summary";

const selectClass = "h-9";

/** A branch as the picker names it: "main · protected". */
export const branchLabel = (b: Pick<Branch, "name" | "protected">) => (b.protected ? `${b.name} · protected` : b.name);
const isMain = (b: Branch) => b.parent_branch_id === null;

/**
 * The Members hub's Roles tab (spec Part 4): project → database → branch → Postgres database, held
 * in the search params; the branch's roles (owner, reserved names and personal logins never
 * listed, R3) as cards or a list; member counts and personal logins for Admins only (MH-34, P24);
 * and "Create role" for Admins.
 */
export function RolesTab() {
  // One source per render: the raw location, parsed with the route's own rules (a param it drops is still seen below).
  const raw = useLocation().search as Record<string, unknown>;
  const search = useMemo(() => validateMembersSearch(raw), [raw]);
  const navigate = useNavigate({ from: "/app/members" });
  const queryClient = useQueryClient();
  const ids = { project: useId(), database: useId(), branch: useId(), pg: useId() };
  const me = useGetMe();
  const projects = useMemo(() => me.data?.tenant?.projects ?? [], [me.data]);

  // Each level falls back to its first (or main) choice; a param that isn't the caller's is ignored.
  const project =
    projects.find((p) => p.id === search.project) ?? projects.find((p) => p.databases.some((d) => d.database_id === search.database)) ?? projects[0];
  const databases = project?.databases ?? [];
  const database = databases.find((d) => d.database_id === search.database) ?? databases[0];
  const dbId = database?.database_id ?? "";
  const branches = useListBranches(dbId, { query: { enabled: dbId !== "" } });
  const branchList = branches.data ?? [];
  const branch = branchList.find((b) => b.id === search.branch) ?? branchList.find(isMain) ?? branchList[0];
  const branchId = branch?.id ?? "";
  const pgDatabases = useListBranchDatabases(dbId, branchId, { query: { enabled: branchId !== "" } });
  const pgList = pgDatabases.data ?? [];
  const pg = pgList.find((d) => d.name === search.pg_database) ?? pgList.find((d) => d.main) ?? pgList[0];
  const roles = useListRoles(dbId, branchId, { query: { enabled: branchId !== "" } });

  const admin = project ? projectCaps(me.data, project.id, dbId || undefined).atLeast("admin") : false;
  const grants = useListProjectGrants(project?.id ?? "", { query: { enabled: admin && project !== undefined } });
  const members = useListMembers({ query: { enabled: admin } });
  const logins = useListDatabasePersonalLogins(dbId, { query: { enabled: admin && dbId !== "" } });

  const [storedView, storeView] = useViewPreference("roles");
  const view: RosterView = search.view ?? storedView;
  const [filter, setFilter] = useState("");
  const [creating, setCreating] = useState(false);

  const set = (patch: Partial<MembersSearch>) =>
    void navigate({ search: (prev: Record<string, unknown>) => ({ ...prev, tab: "roles", ...patch }), replace: true });

  // A param the picker ignored (malformed, not the caller's, not this database's) leaves the URL, so
  // the address shows what is on screen. Each level is judged once its list is known.
  const stale = me.data
    ? [
        raw.view !== undefined && search.view === undefined && "view",
        raw.project !== undefined && raw.project !== project?.id && "project",
        raw.database !== undefined && raw.database !== database?.database_id && "database",
        raw.branch !== undefined && branches.data !== undefined && raw.branch !== branch?.id && "branch",
        raw.pg_database !== undefined && pgDatabases.data !== undefined && raw.pg_database !== pg?.name && "pg_database",
      ].filter((k): k is string => typeof k === "string")
    : [];
  const staleKey = stale.join(",");
  useEffect(() => {
    if (staleKey === "") return;
    const drop = new Set(staleKey.split(","));
    void navigate({ search: (prev: Record<string, unknown>) => Object.fromEntries(Object.entries(prev).filter(([k]) => !drop.has(k))), replace: true });
  }, [staleKey, navigate]);

  if (me.isError) {
    return (
      <p role="alert" className="text-sm text-coral">
        Couldn't load your access: {errorText(me.error)}
      </p>
    );
  }
  if (!me.data) return <p className="text-sm text-chalk-dim">Loading roles…</p>;
  if (!project || !database) return <p className="text-sm text-chalk-dim">No database to show roles for yet. Create one in a project first.</p>;

  const rows = roles.data ?? [];
  const managed = managedRoles(rows);
  const q = filter.trim().toLowerCase();
  const shown = q === "" ? managed : managed.filter((r) => r.name.toLowerCase().includes(q) || escapeName(r.name).toLowerCase().includes(q));
  const counts = admin && grants.data ? new Map(managed.map((r) => [r.name, holdersOf(r.name, grants.data, dbId, rows).length])) : null;
  const where = `${project.name} · ${database.name} · ${branch?.name ?? "main"}`;

  return (
    <div className="space-y-5">
      <div className="grid grid-cols-2 items-end gap-2.5 md:grid-cols-[repeat(4,minmax(0,1fr))_auto]">
        <Picker id={ids.project} label="Project">
          <Select id={ids.project} className={selectClass} value={project.id} onChange={(e) => set({ project: e.target.value, database: undefined, branch: undefined, pg_database: undefined })}>
            {projects.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </Select>
        </Picker>
        <Picker id={ids.database} label="Database">
          <Select
            id={ids.database}
            className={selectClass}
            value={dbId}
            onChange={(e) => set({ project: project.id, database: e.target.value, branch: undefined, pg_database: undefined })}
          >
            {databases.map((d) => (
              <option key={d.database_id} value={d.database_id}>
                {d.name}
              </option>
            ))}
          </Select>
        </Picker>
        <Picker id={ids.branch} label="Branch">
          <Select
            id={ids.branch}
            className={selectClass}
            value={branchId}
            disabled={branchList.length === 0}
            onChange={(e) => set({ project: project.id, database: dbId, branch: e.target.value, pg_database: undefined })}
          >
            {branchList.map((b) => (
              <option key={b.id} value={b.id}>
                {branchLabel(b)}
              </option>
            ))}
          </Select>
        </Picker>
        <Picker id={ids.pg} label="Postgres database">
          <Select
            id={ids.pg}
            className={cn(selectClass, "font-mono")}
            value={pg?.name ?? ""}
            disabled={pgList.length === 0}
            onChange={(e) => set({ project: project.id, database: dbId, branch: branchId, pg_database: e.target.value })}
          >
            {pgList.map((d) => (
              <option key={d.name} value={d.name}>
                {escapeName(d.name)}
              </option>
            ))}
          </Select>
        </Picker>
        {admin && (
          <Button className="col-span-2 h-9 md:col-span-1" disabled={!branch} onClick={() => setCreating(true)}>
            <Plus aria-hidden="true" strokeWidth={1.5} />
            Create role
          </Button>
        )}
      </div>

      {branches.isError && (
        <p role="alert" className="text-sm text-coral">
          Couldn't load branches: {errorText(branches.error)}
        </p>
      )}

      {branch && (
        <div className="flex items-start gap-2.5 border border-line-strong bg-deep px-3.5 py-3 text-[13px] text-chalk-dim">
          {branch.protected && <Lock aria-hidden="true" size={16} strokeWidth={1.5} className="mt-px shrink-0" />}
          <p>
            {branch.protected
              ? `${branch.name} is protected: role and privilege changes here need Admin, and Developers' logins can't change the schema. Roles live on this branch only; to have one elsewhere, merge or reset the other branch.`
              : "Roles live on this branch only; to have one elsewhere, merge or reset the other branch."}
          </p>
        </div>
      )}

      {roles.isError ? (
        <p role="alert" className="text-sm text-coral">
          Couldn't load roles: {errorText(roles.error)}
        </p>
      ) : !roles.data ? (
        branch && <p className="text-sm text-chalk-dim">Loading roles…</p>
      ) : (
        <div className="space-y-3">
          <div className="flex flex-wrap items-center gap-2.5">
            <span className="mr-auto text-sm text-chalk-dim">
              {plural(managed.length, "role")} on {branch?.name}
            </span>
            <div className="relative w-full sm:w-60">
              <Search aria-hidden="true" size={14} strokeWidth={1.5} className="pointer-events-none absolute top-1/2 left-2 -translate-y-1/2 text-chalk-dim" />
              <input
                type="search"
                aria-label="Filter roles"
                placeholder="Filter roles"
                value={filter}
                onChange={(e) => setFilter(e.target.value)}
                className="h-8 w-full border border-input bg-ink pr-2 pl-7 text-sm text-chalk outline-none placeholder:text-chalk-dim focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
              />
            </div>
            <ViewToggle
              value={view}
              onChange={(next) => {
                storeView(next);
                set({ view: next });
              }}
            />
          </div>
          {managed.length === 0 ? (
            <p className="text-sm text-chalk-dim">This branch has no roles of its own yet.{admin && " Create one to group privileges for members and apps."}</p>
          ) : shown.length === 0 ? (
            <p className="text-sm text-chalk-dim">No role matches “{escapeName(filter.trim())}”.</p>
          ) : view === "cards" ? (
            <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
              {shown.map((r) => (
                <RoleLink key={r.name} databaseId={dbId} branchId={branchId} role={r} pg={pg?.name} className="flex min-w-0 flex-col gap-2.5 border border-line p-3.5 hover:border-line-strong hover:bg-chalk/[0.06]">
                  <span className="truncate font-mono text-sm font-medium text-chalk">{escapeName(r.name)}</span>{" "}
                  <span className="flex flex-wrap items-center gap-1.5 text-[13px] text-chalk-dim">
                    <RoleTags role={r} />
                    {counts && <span> · {membersText(counts.get(r.name) ?? 0)}</span>}
                  </span>{" "}
                  <Summary databaseId={dbId} branchId={branchId} pg={pg?.name} role={r} />
                </RoleLink>
              ))}
            </div>
          ) : (
            <RoleTable roles={shown} counts={counts} databaseId={dbId} branchId={branchId} pg={pg?.name} />
          )}
        </div>
      )}

      {admin && (
        <PersonalLogins databaseName={database.name} logins={logins.data?.logins} error={logins.error} members={members.data} />
      )}

      {admin && branch && (
        <CreateRoleDialog
          open={creating}
          onOpenChange={setCreating}
          databaseId={dbId}
          branchId={branchId}
          where={where}
          roles={rows}
          onCreated={(name) => {
            setCreating(false);
            void queryClient.invalidateQueries({ queryKey: getListRolesQueryKey(dbId, branchId) });
            void navigate({
              to: "/app/members/roles/$databaseId/$branchId/$role",
              params: { databaseId: dbId, branchId, role: name },
              search: pg ? { pg_database: pg.name } : {},
            });
          }}
        />
      )}
    </div>
  );
}

function Picker({ id, label, children }: { id: string; label: string; children: ReactNode }) {
  return (
    <div className="min-w-0 space-y-1">
      <label htmlFor={id} className="block text-xs text-chalk-dim">
        {label}
      </label>
      {children}
    </div>
  );
}

function RoleTags({ role }: { role: BranchRole }) {
  return (
    <>
      <Tag>{role.login ? "LOGIN" : "NOLOGIN"}</Tag>
      {role.level === "owner_level" && (
        <>
          {" "}
          <Tag hazard>owner-level</Tag>
        </>
      )}
    </>
  );
}

function RoleLink({ databaseId, branchId, role, pg, className, children }: { databaseId: string; branchId: string; role: BranchRole; pg: string | undefined; className: string; children: ReactNode }) {
  return (
    <Link
      to="/app/members/roles/$databaseId/$branchId/$role"
      params={{ databaseId, branchId, role: role.name }}
      search={pg ? { pg_database: pg } : {}}
      className={cn("outline-none focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand", className)}
    >
      {children}
    </Link>
  );
}

function Summary({ databaseId, branchId, pg, role }: { databaseId: string; branchId: string; pg: string | undefined; role: BranchRole }) {
  const summary = usePrivilegeSummary(databaseId, branchId, pg, role);
  return (
    <span className="line-clamp-2 text-[13px] text-chalk-dim">
      {summary.data ?? (summary.isError ? "Privileges couldn't be read" : summary.isPending && pg ? "Reading privileges…" : "")}
    </span>
  );
}

/**
 * The list view. Each row is one link (the whole row opens the role) laid over the header's columns,
 * so its name reads the row: name, tags, members and the summary.
 */
function RoleTable({ roles, counts, databaseId, branchId, pg }: { roles: BranchRole[]; counts: Map<string, number> | null; databaseId: string; branchId: string; pg: string | undefined }) {
  // Below sm the Members column is hidden (its count moves under the name), so the grid is three columns there.
  const cols = counts
    ? "grid-cols-[minmax(0,36%)_minmax(0,16%)_minmax(0,48%)] sm:grid-cols-[minmax(0,32%)_minmax(0,14%)_minmax(0,14%)_minmax(0,40%)]"
    : "grid-cols-[minmax(0,36%)_minmax(0,16%)_minmax(0,48%)]";
  const head = "px-2.5 py-2 text-left text-xs font-normal text-chalk-dim";
  return (
    <div className="relative overflow-x-auto border border-line">
      <div role="table" aria-label="Roles" className="min-w-[36rem] text-sm">
        <div role="rowgroup" className="border-b border-line-strong bg-channel">
          <div role="row" className={cn("grid", cols)}>
            <div role="columnheader" className={head}>
              Role
            </div>
            <div role="columnheader" className={head}>
              Login
            </div>
            {counts && (
              <div role="columnheader" className={cn(head, "hidden sm:block")}>
                Members
              </div>
            )}
            <div role="columnheader" className={head}>
              Privileges
            </div>
          </div>
        </div>
        <div role="rowgroup">
          {roles.map((r) => (
            <div role="row" key={r.name} className="border-b border-line last:border-0 even:bg-chalk/[0.045]">
              <div role="cell">
                <RoleLink databaseId={databaseId} branchId={branchId} role={r} pg={pg} className={cn("grid items-center hover:bg-chalk/[0.06]", cols)}>
                  <span className="min-w-0 px-2.5 py-2">
                    <span className="flex min-w-0 items-center gap-1.5">
                      <span className="truncate font-mono text-[13px] text-chalk">{escapeName(r.name)}</span>{" "}
                      {r.level === "owner_level" && <Tag hazard>owner-level</Tag>}
                    </span>{" "}
                    {counts && <span className="block text-xs text-chalk-dim sm:hidden">{membersText(counts.get(r.name) ?? 0)}</span>}
                  </span>{" "}
                  <span className="px-2.5 py-2">
                    <Tag>{r.login ? "LOGIN" : "NOLOGIN"}</Tag>
                  </span>{" "}
                  {counts && <span className="hidden px-2.5 py-2 text-[13px] text-chalk-dim sm:block">{membersText(counts.get(r.name) ?? 0)}</span>}{" "}
                  <span className="min-w-0 px-2.5 py-2">
                    <Summary databaseId={databaseId} branchId={branchId} pg={pg} role={r} />
                  </span>
                </RoleLink>
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

/** P24: the database's active personal logins, each with its member (a link to their page) and expiry. */
function PersonalLogins({ databaseName, logins, error, members }: { databaseName: string; logins: PersonalLoginView[] | undefined; error: unknown; members: Member[] | undefined }) {
  const active = (logins ?? []).filter((l) => l.status === "active").sort((a, b) => a.role_name.localeCompare(b.role_name));
  const byId = new Map((members ?? []).map((m) => [m.id, m]));
  // Shown once read, so its heading's count is never a placeholder.
  if (!logins && !error) return null;
  const row = "grid grid-cols-[minmax(0,1fr)_minmax(0,1.4fr)_minmax(0,1fr)] items-center";
  return (
    <section className="space-y-3 pt-4">
      <div className="space-y-1">
        <h2 className="text-base font-semibold">
          Personal logins on {databaseName} <span className="font-normal text-chalk-dim">{active.length}</span>
        </h2>
        <p className="text-[13px] text-chalk-dim">Members' own logins. They aren't roles you grant: each follows its member's access, on every branch.</p>
      </div>
      {error ? (
        <p className="text-sm text-coral">Couldn't load personal logins: {errorText(error)}</p>
      ) : active.length === 0 ? (
        <p className="text-sm text-chalk-dim">No member has a personal login on {databaseName}.</p>
      ) : (
        <div className="relative overflow-x-auto border border-line text-sm">
          <div aria-hidden="true" className={cn(row, "min-w-[28rem] border-b border-line-strong bg-channel text-xs text-chalk-dim")}>
            <span className="px-2.5 py-2">Login</span>
            <span className="px-2.5 py-2">Member</span>
            <span className="px-2.5 py-2">Expires</span>
          </div>
          <ul className="min-w-[28rem]">
            {active.map((l) => {
              const m = byId.get(l.member_id);
              const soon = phaseOf(l) === "soon";
              return (
                <li key={l.member_id} className={cn(row, "border-b border-line last:border-0 even:bg-chalk/[0.045]")}>
                  <span className="truncate px-2.5 py-2 font-mono text-[13px]">{escapeName(l.role_name)}</span>{" "}
                  <span className="truncate px-2.5 py-2">
                    {m ? (
                      <Link to="/app/members/$memberId" params={{ memberId: m.id }} className="text-chalk underline decoration-line-strong underline-offset-4 hover:decoration-chalk">
                        {memberName(m)}
                      </Link>
                    ) : (
                      <span className="text-chalk-dim">Unknown member</span>
                    )}
                  </span>{" "}
                  <span className="flex items-center gap-2 px-2.5 py-2 text-[13px] text-chalk-dim">
                    {soon && (
                      <span title="Expires within 14 days" className="size-2 shrink-0 border border-coral">
                        <span className="sr-only">Expires soon:</span>
                      </span>
                    )}
                    <span>{daysFrom(l.valid_until)}</span>
                  </span>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </section>
  );
}
