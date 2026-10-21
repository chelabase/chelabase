import type { StatusReason } from "@/lib/realtime/types";

/**
 * The "Live" dot's colour: a signal green, not a presence colour, so it never reads as a member's avatar. It is at
 * least 14 (OKLab x100) from palm and 17 from seafoam, its nearest neighbours, and 5.7:1 or more on ink, deep and
 * channel in both themes (live-badge.test.tsx pins at least 8 and 3:1).
 */
export const LIVE_COLOR = "#22c55e";

/** Why live updates are paused, as the header says it (§6.1). */
export const PAUSED: Record<StatusReason | "forbidden" | "not_found" | "limit" | "invalid", string> = {
  no_slot: "no free slot",
  waiting: "waiting for a slot",
  no_connect: "can't connect to the database",
  read_only: "read-only compute",
  not_a_table: "not a table",
  unavailable: "unavailable",
  compute_stopping: "the compute is stopping",
  reconnecting: "reconnecting",
  unsupported: "not supported for this database",
  forbidden: "no access",
  not_found: "not found",
  limit: "too many live tables",
  invalid: "unavailable",
};

export type LiveStatus = { live: boolean; reason?: keyof typeof PAUSED };

/**
 * The table bar's live state. Live: a green dot with a ring around it (a slow pulse; a still ring under reduced
 * motion), so the state doesn't rest on colour alone, and "Live". Paused: a hollow dot and "Live updates paused" with
 * its reason.
 */
export function LiveBadge({ status }: { status: LiveStatus }) {
  return (
    <span data-testid="live-state" className="flex items-center gap-1.5 text-[13px] whitespace-nowrap text-chalk-dim">
      {status.live ? (
        <span data-testid="live-dot" data-color={LIVE_COLOR} data-cue="ring" aria-hidden="true" className="relative size-2 shrink-0" style={{ background: LIVE_COLOR }}>
          <span
            data-testid="live-ring"
            aria-hidden="true"
            className="absolute -inset-[3px] animate-ping border opacity-60 [animation-duration:2s] motion-reduce:animate-none"
            style={{ borderColor: LIVE_COLOR }}
          />
        </span>
      ) : (
        <span data-testid="live-dot" aria-hidden="true" className="size-2 shrink-0 border border-chalk-dim" />
      )}
      {status.live ? "Live" : `Live updates paused · ${PAUSED[status.reason ?? "unavailable"] ?? PAUSED.unavailable}`}
    </span>
  );
}
