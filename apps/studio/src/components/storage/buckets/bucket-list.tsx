import type { MouseEvent } from "react";
import { formatBytes } from "@/lib/table-editor/dry-run";
import { objectCountText, useBucketStats, type Bucket, type BucketStats } from "@/lib/storage-api";
import { cn } from "@/lib/utils";

export interface BucketListProps {
  databaseId: string;
  branchId: string;
  role: string | null;
  buckets: Bucket[];
  selected: string | undefined;
  /** The bucket's URL (the link's href). */
  hrefFor: (name: string) => string;
  onSelect: (name: string) => void;
}

/** Public or private, as a small tag: public is seafoam (open), private stays quiet. */
export function AccessBadge({ isPublic }: { isPublic: boolean }) {
  return (
    <span className={cn("shrink-0 border px-1.5 text-[11px] leading-5", isPublic ? "border-seafoam/60 text-seafoam" : "border-line-strong text-chalk-dim")}>
      {isPublic ? "Public" : "Private"}
    </span>
  );
}

/** "2 objects · 3 kB", with a plus when the listing was cut at a page. */
export function statsText(s: BucketStats): string {
  return `${objectCountText(s)} · ${formatBytes(s.bytes)}${s.more ? "+" : ""}`;
}

function BucketRow({ databaseId, branchId, role, bucket, current, href, onSelect }: { databaseId: string; branchId: string; role: string | null; bucket: Bucket; current: boolean; href: string; onSelect: (name: string) => void }) {
  const stats = useBucketStats(databaseId, branchId, role, bucket.name, true);
  function click(event: MouseEvent<HTMLAnchorElement>) {
    if (event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) return;
    event.preventDefault();
    onSelect(bucket.name);
  }
  return (
    <li>
      <a
        href={href}
        onClick={click}
        aria-current={current ? "page" : undefined}
        className={cn(
          "relative flex flex-col gap-0.5 px-4 py-2 text-sm transition-colors hover:bg-chalk/[0.06]",
          "focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-sand",
          current && "bg-chalk/[0.04] before:absolute before:inset-y-1.5 before:left-0 before:w-0.5 before:bg-sand",
        )}
      >
        <span className="flex items-center gap-2">
          <span className="min-w-0 flex-1 truncate font-mono">{bucket.name}</span>
          <AccessBadge isPublic={bucket.public} />
        </span>
        <span className="text-xs text-chalk-dim tabular-nums">{stats.data ? statsText(stats.data) : stats.isError ? "Size unknown" : "Counting…"}</span>
      </a>
    </li>
  );
}

/** The left pane's bucket list: each with its access, object count and size as the role sees them. */
export function BucketList({ databaseId, branchId, role, buckets, selected, hrefFor, onSelect }: BucketListProps) {
  return (
    <ul aria-label="Buckets" className="flex flex-col py-1">
      {buckets.map((b) => (
        <BucketRow key={b.id} databaseId={databaseId} branchId={branchId} role={role} bucket={b} current={b.name === selected} href={hrefFor(b.name)} onSelect={onSelect} />
      ))}
    </ul>
  );
}
