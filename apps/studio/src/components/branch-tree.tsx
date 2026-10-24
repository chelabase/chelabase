import { Fragment, useState } from "react";
import type { Branch, MergeSummary } from "@chelabase/management-client";
import { buildTree } from "@/lib/branches";
import type { ConnectionFocus } from "@/lib/project-search";
import { BranchActions } from "./branch-actions";
import { BranchRoles } from "./branch-roles";

export interface BranchTreeProps {
  projectId: string;
  databaseId: string;
  databaseName: string;
  /** The main Postgres database (`DatabaseSummary.postgres_database`): merges name it. */
  postgresDatabase?: string | null;
  branches: Branch[];
  historyWindow: number;
  /** The database's merges (an open one blocks starting another between the same branches). */
  openMerges?: MergeSummary[];
  /** A branch whose connection details open on load. */
  focus?: ConnectionFocus | null;
}

const utc = (iso: string) => `${new Date(iso).toISOString().slice(0, 16).replace("T", " ")} UTC`;

export function BranchTree({
  projectId,
  databaseId,
  databaseName,
  postgresDatabase = null,
  branches,
  historyWindow,
  openMerges,
  focus = null,
}: BranchTreeProps) {
  const rows = buildTree(branches);
  // The branch whose Roles panel is open: it opens as a full-width row under the branch.
  const [rolesFor, setRolesFor] = useState<string | null>(null);
  return (
    <div className="relative max-w-full overflow-x-auto">
    <table className="w-full text-left text-sm">
      <thead className="border-b text-muted-foreground">
        <tr>
          <th className="py-2 pr-4 font-medium">Branch</th>
          <th className="py-2 pr-4 font-medium">Status</th>
          <th className="py-2 pr-4 font-medium">Endpoint</th>
          <th className="py-2 font-medium">Actions</th>
        </tr>
      </thead>
      <tbody>
        {rows.map(({ branch, depth, parentName }) => {
          const point = branch.branched_at_time ? utc(branch.branched_at_time) : branch.branched_at_lsn;
          return (
            <Fragment key={branch.id}>
            <tr data-branch={branch.name} data-depth={depth} className="border-b align-top last:border-0">
              <td className="py-2 pr-4" style={{ paddingLeft: `${depth * 1.5}rem` }}>
                <span className="font-medium">{branch.name}</span>
                {branch.kind === "backup" && (
                  <span className="ml-2 rounded border px-1.5 py-0.5 text-xs text-muted-foreground">backup</span>
                )}
                {parentName && point && (
                  <div className="text-xs text-muted-foreground">
                    from {parentName} at {point}
                  </div>
                )}
              </td>
              <td className="py-2 pr-4">
                <div>{branch.status}</div>
                {branch.error &&
                  (branch.status === "failed" ? (
                    <div className="max-w-xs text-xs break-words text-destructive">{branch.error}</div>
                  ) : (
                    // The branch works; the error is what the last attempt (a reset, restore or
                    // delete) left, shown as recorded.
                    <div className="max-w-xs text-xs break-words text-muted-foreground">
                      {branch.status === "ready" ? `Last attempt: ${branch.error}` : branch.error}
                    </div>
                  ))}
              </td>
              <td className="py-2 pr-4 font-mono">{branch.host && branch.port ? `${branch.host}:${branch.port}` : "—"}</td>
              <td className="py-2">
                <BranchActions
                  projectId={projectId}
                  openMerges={openMerges}
                  openConnection={focus?.branchId === branch.id ? { role: focus.role } : undefined}
                  databaseId={databaseId}
                  databaseName={databaseName}
                  postgresDatabase={postgresDatabase}
                  branch={branch}
                  branches={branches}
                  historyWindow={historyWindow}
                  roles={{ open: rolesFor === branch.id, onToggle: () => setRolesFor(rolesFor === branch.id ? null : branch.id) }}
                />
              </td>
            </tr>
            {rolesFor === branch.id && (
              <tr className="border-b last:border-0">
                <td colSpan={4} className="pb-3">
                  <BranchRoles projectId={projectId} databaseId={databaseId} branchId={branch.id} />
                </td>
              </tr>
            )}
            </Fragment>
          );
        })}
      </tbody>
    </table>
    </div>
  );
}
