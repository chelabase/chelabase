import { useListBranches, useListMerges, type MergeSummary } from "@chelabase/management-client";
import { errorText } from "@/lib/errors";
import { MERGE_STATUS_LABEL, isMergeBusy, mergeTitle, utcMinute } from "@/lib/merges";
import { pollWhile } from "@/lib/query";
import { MergeLink } from "./merge-link";

export interface MergesListProps {
  projectId: string;
  databaseId: string;
}

/** One database's merges, newest first: open ones and the history. */
export function MergesList({ projectId, databaseId }: MergesListProps) {
  const merges = useListMerges(databaseId, undefined, {
    query: { refetchInterval: pollWhile<MergeSummary[]>((ms) => ms.some((m) => isMergeBusy(m.status))) },
  });
  const branches = useListBranches(databaseId);
  // A deleted branch drops out of the list; its merges still show.
  const name = (id: string) => branches.data?.find((b) => b.id === id)?.name ?? "a deleted branch";

  if (merges.isError) {
    return (
      <p role="alert" className="py-3 text-sm text-destructive">
        Couldn&apos;t load the merges: {errorText(merges.error)}
      </p>
    );
  }
  if (merges.data === undefined) return <p className="py-3 text-sm text-muted-foreground">Loading merges…</p>;
  if (merges.data.length === 0) {
    return (
      <p className="py-3 text-sm text-muted-foreground">
        No merges yet. Start one from a branch: Branches, then Merge into or Update from its parent.
      </p>
    );
  }
  return (
    <table aria-label="Merges" className="my-3 w-full text-left text-sm">
      <thead className="border-b text-muted-foreground">
        <tr>
          <th className="py-2 font-medium">Merge</th>
          <th className="py-2 font-medium">Database</th>
          <th className="py-2 font-medium">Status</th>
          <th className="py-2 font-medium">Started</th>
          <th className="py-2 font-medium">Updated</th>
        </tr>
      </thead>
      <tbody>
        {merges.data.map((m) => (
          <tr key={m.id} className="border-b last:border-0">
            <td className="py-2">
              <MergeLink projectId={projectId} databaseId={databaseId} mergeId={m.id}>
                {mergeTitle(m.direction, name(m.source_branch_id), name(m.target_branch_id))}
              </MergeLink>
            </td>
            <td className="py-2 font-mono">{m.database}</td>
            <td className="py-2">{MERGE_STATUS_LABEL[m.status]}</td>
            <td className="py-2">{utcMinute(m.created_at)}</td>
            <td className="py-2">{utcMinute(m.updated_at)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
