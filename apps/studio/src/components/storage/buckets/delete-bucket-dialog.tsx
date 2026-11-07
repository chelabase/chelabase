import { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { storageListObjects, useStorageDeleteBucket } from "@chelabase/management-client";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { errorText } from "@/lib/errors";
import { STATS_PAGE, bucketStatsKey, invalidateStorage, isBucketNotEmpty, objectCountText, storageParams, summarizeObjects } from "@/lib/storage-api";

export interface DeleteBucketDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  databaseId: string;
  branchId: string;
  role: string | null;
  name: string;
  onDeleted: () => void;
}

/**
 * Deletes an empty bucket. Storage refuses one that still holds objects without a count (it would
 * include objects the role can't see), so the refusal is shown with the count the role does see.
 */
export function DeleteBucketDialog({ open, onOpenChange, databaseId, branchId, role, name, onDeleted }: DeleteBucketDialogProps) {
  const queryClient = useQueryClient();
  const [refusal, setRefusal] = useState<string | null>(null);
  const remove = useStorageDeleteBucket({
    mutation: {
      onSuccess: async () => {
        await invalidateStorage(queryClient, databaseId, branchId);
        onOpenChange(false);
        onDeleted();
      },
      onError: async (err) => {
        if (!isBucketNotEmpty(err)) return;
        const stats = await queryClient
          .fetchQuery({
            queryKey: bucketStatsKey(databaseId, branchId, role, name),
            queryFn: async () => summarizeObjects(await storageListObjects(databaseId, branchId, name, { prefix: "", limit: STATS_PAGE }, storageParams(role))),
            staleTime: 0,
          })
          .catch(() => null);
        setRefusal(
          stats && stats.objects > 0
            ? `${name} still holds ${objectCountText(stats)}; delete them first.`
            : `${name} still holds objects your role can't see; delete them first.`,
        );
      },
    },
  });
  useEffect(() => {
    if (open) {
      setRefusal(null);
      remove.reset();
    }
    // Reset on each opening only.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const error = refusal ?? (remove.error && !isBucketNotEmpty(remove.error) ? errorText(remove.error) : null);
  return (
    <ConfirmDialog
      open={open}
      onOpenChange={onOpenChange}
      title={`Delete ${name}?`}
      body={<p>Only an empty bucket can be deleted. This can&apos;t be undone.</p>}
      confirm={`Delete ${name}`}
      tone="destructive"
      busy={remove.isPending}
      error={error}
      onConfirm={() => remove.mutate({ id: databaseId, branchId, name, params: storageParams(role) })}
    />
  );
}
