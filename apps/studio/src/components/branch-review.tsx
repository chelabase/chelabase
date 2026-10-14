import { useEffect, useRef, useState } from "react";
import { useQueryClient, type QueryClient } from "@tanstack/react-query";
import {
  ApiError,
  getGetBranchReviewQueryKey,
  useFixBranchReviewItem,
  type FixAction,
  type Review,
  type ReviewItem,
  type ReviewReason,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { invalidateBranches } from "@/lib/branches";
import { errorText } from "@/lib/errors";

/** `schema.name(identity)` for a function, `name on identity` for a trigger or a rule. */
export function itemLabel(item: ReviewItem): string {
  return item.kind === "function" ? `${item.schema}.${item.name}(${item.identity})` : `${item.name} on ${item.identity}`;
}

/** A reason's kind with `_` as a space, then what it came via. */
export function reasonText(reason: ReviewReason): string {
  const kind = reason.kind.replace(/_/g, " ");
  return reason.via ? `${kind} ${reason.via}` : kind;
}

export const REVIEW_CHANGED_TEXT = "The review changed since you loaded it. Check the list again.";

/**
 * A `review_changed` error: the review it carries replaces the cached one. Returns the text to show,
 * or null for any other error.
 */
export function reviewChangedText(err: unknown, queryClient: QueryClient, databaseId: string, branchId: string): string | null {
  if (!(err instanceof ApiError) || err.code !== "review_changed") return null;
  if (err.extra.review) queryClient.setQueryData(getGetBranchReviewQueryKey(databaseId, branchId), err.extra.review as Review);
  return REVIEW_CHANGED_TEXT;
}

export const REASSIGN_WARNING = "The owner takes this code over. A SECURITY DEFINER function then runs as the owner.";

export interface BranchReviewProps {
  databaseId: string;
  branchId: string;
  review: Review;
  /** Show the Reassign and Drop buttons. */
  canFix: boolean;
}

/** The review's list: each item with its reasons and, for an Admin, Reassign and Drop (each confirmed). */
export function BranchReview({ databaseId, branchId, review, canFix }: BranchReviewProps) {
  const queryClient = useQueryClient();
  const fix = useFixBranchReviewItem();
  const [pending, setPending] = useState<{ item: ReviewItem; action: FixAction } | null>(null);
  const [ran, setRan] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function confirm() {
    if (!pending) return;
    setError(null);
    setRan(null);
    try {
      const done = await fix.mutateAsync({
        id: databaseId,
        branchId,
        data: { key: pending.item.key, action: pending.action, hash: review.hash },
      });
      queryClient.setQueryData(getGetBranchReviewQueryKey(databaseId, branchId), done.review);
      void invalidateBranches(queryClient, databaseId);
      setRan(done.sql);
      setPending(null);
    } catch (err) {
      setPending(null);
      setError(reviewChangedText(err, queryClient, databaseId, branchId) ?? errorText(err));
    }
  }
  // The confirm takes focus when it opens.
  const confirmRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (pending) confirmRef.current?.focus();
  }, [pending]);

  if (review.items.length === 0) {
    return <p className="text-muted-foreground">Nothing to review: no code owned by a non-Admin role can run in someone else&apos;s session.</p>;
  }
  return (
    <div className="space-y-2">
      <ul className="space-y-2">
        {review.items.map((item) => {
          const label = itemLabel(item);
          return (
            <li key={item.key} className="space-y-1 border p-2">
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono">{label}</span>
                <span className="text-xs text-muted-foreground">
                  {item.kind} in {item.database}, owned by {item.owner}
                </span>
                {item.new && <span className="border px-2 py-0.5 text-xs">New</span>}
                {item.opaque_body && <span className="text-xs text-muted-foreground">Body can&apos;t be followed</span>}
              </div>
              <ul className="list-disc pl-5 text-xs">
                {item.reasons.map((r) => (
                  <li key={`${r.kind}/${r.via ?? ""}`}>{reasonText(r)}</li>
                ))}
              </ul>
              {canFix && (
                <div className="flex gap-2">
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={fix.isPending}
                    aria-label={`Reassign ${label}`}
                    onClick={() => setPending({ item, action: "reassign" })}
                  >
                    Reassign
                  </Button>
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={fix.isPending}
                    aria-label={`Drop ${label}`}
                    onClick={() => setPending({ item, action: "drop" })}
                  >
                    Drop
                  </Button>
                </div>
              )}
            </li>
          );
        })}
      </ul>
      {pending && (
        <div className="space-y-2 border p-3">
          <p>
            {pending.action === "reassign"
              ? REASSIGN_WARNING
              : `Drop ${itemLabel(pending.item)}? It is removed from the database. This can't be undone.`}
          </p>
          <div className="flex gap-2">
            <Button ref={confirmRef} size="sm" disabled={fix.isPending} onClick={() => void confirm()}>
              {pending.action === "reassign" ? "Confirm reassign" : "Confirm drop"}
            </Button>
            <Button size="sm" variant="outline" onClick={() => setPending(null)}>
              Cancel
            </Button>
          </div>
        </div>
      )}
      {ran && (
        <div className="space-y-1">
          <p className="text-xs text-muted-foreground">Ran:</p>
          <pre className="overflow-x-auto border p-2 font-mono text-xs">{ran}</pre>
        </div>
      )}
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
    </div>
  );
}
