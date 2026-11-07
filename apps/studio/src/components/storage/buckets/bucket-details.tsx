import { useState } from "react";
import { Button } from "@/components/ui/button";
import { formatBytes } from "@/lib/table-editor/dry-run";
import { useBucketStats, type Bucket } from "@/lib/storage-api";
import { BucketDialog } from "./bucket-dialog";
import { AccessBadge, statsText } from "./bucket-list";
import { DeleteBucketDialog } from "./delete-bucket-dialog";

export interface BucketDetailsProps {
  databaseId: string;
  branchId: string;
  role: string | null;
  bucket: Bucket;
  /** The role may change buckets (Developer or above, as acted). Viewers get no controls at all. */
  canWrite: boolean;
  onDeleted: () => void;
}

/** The selected bucket's settings, with Edit settings and Delete bucket for those who may change it. */
export function BucketDetails({ databaseId, branchId, role, bucket, canWrite, onDeleted }: BucketDetailsProps) {
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const stats = useBucketStats(databaseId, branchId, role, bucket.name, true);
  const rows: [string, string][] = [
    ["Access", bucket.public ? "Public: anyone with a file's URL can download it" : "Private: files need a signed URL or a key"],
    ["Size limit", bucket.file_size_limit === null ? "None" : formatBytes(bucket.file_size_limit)],
    ["Allowed types", bucket.allowed_mime_types === null ? "Any" : bucket.allowed_mime_types.join(", ")],
    ["Write-once", bucket.write_once ? "Yes: files can't be overwritten or deleted" : "No"],
    ["Objects", stats.data ? statsText(stats.data) : stats.isError ? "Unknown" : "Counting…"],
  ];
  return (
    <section aria-labelledby="bucket-title" className="space-y-4 p-6 text-sm">
      <div className="flex flex-wrap items-center gap-3">
        <h2 id="bucket-title" className="font-mono text-lg font-semibold">
          {bucket.name}
        </h2>
        <AccessBadge isPublic={bucket.public} />
        <span className="flex-1" />
        {canWrite && (
          <>
            <Button variant="outline" size="sm" onClick={() => setEditing(true)}>
              Edit settings
            </Button>
            <Button variant="destructive" size="sm" onClick={() => setDeleting(true)}>
              Delete bucket
            </Button>
          </>
        )}
      </div>
      <dl className="grid max-w-2xl grid-cols-[minmax(7rem,auto)_1fr] gap-x-6 gap-y-2 border-t border-line pt-4">
        {rows.map(([term, value]) => (
          <div key={term} className="contents">
            <dt className="text-chalk-dim">{term}</dt>
            <dd className={term === "Allowed types" && bucket.allowed_mime_types !== null ? "font-mono" : undefined}>{value}</dd>
          </div>
        ))}
      </dl>
      {canWrite && (
        <>
          <BucketDialog open={editing} onOpenChange={setEditing} databaseId={databaseId} branchId={branchId} role={role} bucket={bucket} />
          <DeleteBucketDialog open={deleting} onOpenChange={setDeleting} databaseId={databaseId} branchId={branchId} role={role} name={bucket.name} onDeleted={onDeleted} />
        </>
      )}
    </section>
  );
}
