import { useId, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { ApiError, useDeclineMember, type Member, type Project } from "@chelabase/management-client";
import { refreshAccess } from "./keys";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { errorText } from "@/lib/errors";
import { memberName, relativeTime, utc } from "@/lib/members";
import { ApproveDialog } from "./approve-dialog";

export interface RequestsProps {
  pending: readonly Member[];
  projects: readonly Project[];
  onStatus: (message: string) => void;
  /** The last request was handled: the section goes, so focus must go somewhere else. */
  onEmptied: () => void;
}

/** Sign-ups waiting for a tenant admin (R40): approve with project access, or decline. */
export function Requests({ pending, projects, onStatus, onEmptied }: RequestsProps) {
  const headingId = useId();
  const queryClient = useQueryClient();
  const decline = useDeclineMember();
  const [approving, setApproving] = useState<Member | null>(null);
  const [declining, setDeclining] = useState<Member | null>(null);
  const [declineError, setDeclineError] = useState<string | null>(null);
  /** Each request's Approve button, and where the dialogs hand focus back. */
  const approveButtons = useRef(new Map<string, HTMLElement>());
  const returnTo = useRef<HTMLElement | null>(null);
  // A handled request's row goes: focus moves to the next request's Approve, else the previous
  // one's; with none left the caller takes it (the whole section goes).
  const handled = (m: Member) => {
    const at = pending.findIndex((p) => p.id === m.id);
    const neighbour = pending[at + 1] ?? pending[at - 1];
    const next = neighbour ? (approveButtons.current.get(neighbour.id) ?? null) : null;
    returnTo.current = next;
    if (!next) onEmptied();
  };

  async function confirmDecline() {
    if (!declining) return;
    setDeclineError(null);
    try {
      await decline.mutateAsync({ memberId: declining.id });
      onStatus(`Declined ${declining.email ?? memberName(declining)}.`);
      handled(declining);
      setDeclining(null);
    } catch (err) {
      setDeclineError(
        err instanceof ApiError && err.code === "conflict"
          ? `${memberName(declining)} was approved or declined in the meantime.`
          : errorText(err),
      );
    } finally {
      refreshAccess(queryClient);
    }
  }

  return (
    <section aria-labelledby={headingId} className="space-y-3">
      <div className="space-y-1">
        <h2 id={headingId} className="flex items-baseline gap-2 text-base font-semibold">
          Requests <span className="text-sm font-normal text-chalk-dim tabular-nums">{pending.length}</span>
        </h2>
        <p className="max-w-[65ch] text-sm text-chalk-dim">
          These people signed up and are waiting for a tenant admin. Approve them with the projects they need, or decline.
        </p>
      </div>
      <ul className="border-y border-line">
        {pending.map((m) => {
          const name = memberName(m);
          return (
            <li key={m.id} className="flex min-h-12 flex-wrap items-center gap-x-4 gap-y-1 border-b border-line py-2 last:border-b-0">
              <div className="min-w-0 flex-1">
                <div className="truncate text-sm text-chalk">{name}</div>
                <div className="truncate text-sm text-chalk-dim">{m.email ?? "Email unknown"}</div>
              </div>
              <div className="text-sm text-chalk-dim tabular-nums">
                Signed up <time dateTime={m.created_at} title={utc(m.created_at)}>{relativeTime(m.created_at)}</time>
              </div>
              <div className="flex gap-2">
                <Button
                  ref={(el: HTMLElement | null) => {
                    if (el) approveButtons.current.set(m.id, el);
                    else approveButtons.current.delete(m.id);
                  }}
                  size="sm"
                  variant="outline"
                  aria-label={`Approve ${name}`}
                  onClick={(e) => {
                    returnTo.current = e.currentTarget;
                    setApproving(m);
                  }}
                >
                  Approve…
                </Button>
                <Button
                  size="sm"
                  variant="destructive"
                  aria-label={`Decline ${name}`}
                  onClick={(e) => {
                    returnTo.current = e.currentTarget;
                    setDeclineError(null);
                    setDeclining(m);
                  }}
                >
                  Decline
                </Button>
              </div>
            </li>
          );
        })}
      </ul>
      <ApproveDialog
        member={approving}
        onClose={() => setApproving(null)}
        projects={projects}
        finalFocus={returnTo}
        onDone={(message) => {
          if (approving) handled(approving);
          setApproving(null);
          onStatus(message);
        }}
      />
      <ConfirmDialog
        finalFocus={returnTo}
        open={declining !== null}
        onOpenChange={(open) => !open && setDeclining(null)}
        title={`Decline ${declining ? memberName(declining) : ""}?`}
        body="Their sign-up is removed and their account is deactivated, so they can't sign in. To let them in later, invite them."
        confirm="Decline"
        tone="destructive"
        busy={decline.isPending}
        error={declineError}
        onConfirm={() => void confirmDecline()}
      />
    </section>
  );
}
