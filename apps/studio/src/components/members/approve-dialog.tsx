import { useState, type FormEvent, type RefObject } from "react";
import { useIsMutating, useQueryClient } from "@tanstack/react-query";
import { ApiError, getApproveMemberMutationKey, useApproveMember, type Member, type Project } from "@chelabase/management-client";
import { refreshAccess } from "./keys";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { memberName } from "@/lib/members";
import { GrantRows, toInviteGrants, type GrantRow } from "./grant-rows";
import { inviteFailure, type InviteFailure } from "./invite-errors";

export interface ApproveDialogProps {
  /** The pending member being approved; null closes the dialog. */
  member: Member | null;
  onClose: () => void;
  projects: readonly Project[];
  onDone: (message: string) => void;
  /** Where focus goes when it closes (default: the trigger). */
  finalFocus?: RefObject<HTMLElement | null>;
}

/** Approve a sign-up request with its first project access (R40). */
export function ApproveDialog({ member, onClose, projects, onDone, finalFocus }: ApproveDialogProps) {
  const name = member ? memberName(member) : "";
  // No closing while the approval is in flight.
  const busy = useIsMutating({ mutationKey: getApproveMemberMutationKey() }) > 0;
  return (
    <Modal
      open={member !== null}
      onOpenChange={(open) => !open && !busy && onClose()}
      title={`Approve ${name}`}
      description={member?.email ?? undefined}
      size="wide"
      finalFocus={finalFocus}
    >
      {member && <ApproveForm key={member.id} member={member} projects={projects} onCancel={onClose} onDone={onDone} />}
    </Modal>
  );
}

function ApproveForm({ member, projects, onCancel, onDone }: { member: Member; projects: readonly Project[]; onCancel: () => void; onDone: (m: string) => void }) {
  const queryClient = useQueryClient();
  const approve = useApproveMember();
  const [rows, setRows] = useState<GrantRow[]>([]);
  const [failure, setFailure] = useState<InviteFailure>({});
  const email = member.email ?? memberName(member);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (approve.isPending) return;
    setFailure({});
    try {
      await approve.mutateAsync({ memberId: member.id, data: { grants: toInviteGrants(rows) } });
      refreshAccess(queryClient);
      onDone(`Approved ${email}. They can sign in now.`);
    } catch (err) {
      if (err instanceof ApiError && err.code === "conflict") {
        refreshAccess(queryClient);
        setFailure({ form: `${memberName(member)} was approved or declined in the meantime.` });
      } else {
        setFailure(inviteFailure(err, email, rows, projects));
      }
    }
  }

  const busy = approve.isPending;
  return (
    <form onSubmit={submit} className="space-y-5">
      <GrantRows projects={projects} rows={rows} onChange={setRows} errors={failure.rows} disabled={busy} />
      {(failure.form ?? failure.email) && (
        <p role="alert" className="text-sm text-coral">
          {failure.form ?? failure.email}
        </p>
      )}
      <div className="flex justify-end gap-2">
        <Button type="button" variant="ghost" onClick={onCancel} disabled={busy}>
          Cancel
        </Button>
        <Button type="submit" disabled={busy || rows.some((r) => r.blocked)}>
          {busy ? "Approving…" : "Approve"}
        </Button>
      </div>
    </form>
  );
}
