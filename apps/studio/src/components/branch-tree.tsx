import type { Branch } from "@cheladb/management-client";
import { buildTree } from "@/lib/branches";
import { BranchActions } from "./branch-actions";

export interface BranchTreeProps {
  databaseId: string;
  databaseName: string;
  branches: Branch[];
  historyWindow: number;
}

const utc = (iso: string) => `${new Date(iso).toISOString().slice(0, 16).replace("T", " ")} UTC`;

export function BranchTree({ databaseId, databaseName, branches, historyWindow }: BranchTreeProps) {
  const rows = buildTree(branches);
  return (
    <table className="w-full text-left text-sm">
      <thead className="border-b text-muted-foreground">
        <tr>
          <th className="py-2 font-medium">Branch</th>
          <th className="py-2 font-medium">Status</th>
          <th className="py-2 font-medium">Endpoint</th>
          <th className="py-2 font-medium">Actions</th>
        </tr>
      </thead>
      <tbody>
        {rows.map(({ branch, depth, parentName }) => {
          const point = branch.branched_at_time ? utc(branch.branched_at_time) : branch.branched_at_lsn;
          return (
            <tr key={branch.id} data-branch={branch.name} data-depth={depth} className="border-b align-top last:border-0">
              <td className="py-2" style={{ paddingLeft: `${depth * 1.5}rem` }}>
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
              <td className="py-2">
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
              <td className="py-2 font-mono">{branch.host && branch.port ? `${branch.host}:${branch.port}` : "—"}</td>
              <td className="py-2">
                <BranchActions
                  databaseId={databaseId}
                  databaseName={databaseName}
                  branch={branch}
                  branches={branches}
                  historyWindow={historyWindow}
                />
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
