import { useEffect, useId, useRef, useState, type FormEvent, type RefObject } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { createInvite, useGetInviteDelivery, type Delivery, type InviteDelivery, type Project } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { DeliveryField } from "./delivery-field";
import { GrantRows, toInviteGrants, type GrantRow } from "./grant-rows";
import { inviteFailure, looksLikeEmail, type InviteFailure } from "./invite-errors";
import type { OneTimeLink } from "./invite-link-panel";
import { refreshAccess } from "./keys";

export interface InviteDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  projects: readonly Project[];
  /** The dialog closed after a success: what to say under the header. */
  onDone: (message: string) => void;
  /** A link invite was made: the caller shows it once (the dialog has closed). */
  onLink: (link: OneTimeLink) => void;
  finalFocus?: RefObject<HTMLElement | null>;
}

const field =
  "h-8 w-full border border-input bg-ink px-2 text-sm text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand aria-[invalid=true]:border-coral";

/** A lookup that failed counts as "no email": the link always works. */
const UNKNOWN: InviteDelivery = { email_available: false, reason: "zitadel_unavailable" };

/** "Invite people" (tenant admins): an email, initial project access and the delivery. */
export function InviteDialog({ open, onOpenChange, projects, onDone, onLink, finalFocus }: InviteDialogProps) {
  const [busy, setBusy] = useState(false);
  // Closed because a link was made: the link dialog has taken focus, so this one must not hand it
  // back to the trigger when its exit transition ends (that would land behind the open modal).
  const linked = useRef(false);
  useEffect(() => {
    if (open) linked.current = false;
  }, [open]);
  return (
    <Modal
      open={open}
      // No closing while the request is in flight.
      onOpenChange={(next) => !busy && onOpenChange(next)}
      title="Invite people"
      size="wide"
      finalFocus={() => (linked.current ? false : (finalFocus?.current ?? true))}
    >
      {/* Mounted only while open: closing drops the form and the errors. */}
      {open && (
        <InviteForm
          projects={projects}
          onBusy={setBusy}
          onCancel={() => onOpenChange(false)}
          onDone={onDone}
          onLink={(l) => {
            linked.current = true;
            onLink(l);
          }}
        />
      )}
    </Modal>
  );
}

function InviteForm({
  projects,
  onBusy,
  onCancel,
  onDone,
  onLink,
}: {
  projects: readonly Project[];
  onBusy: (busy: boolean) => void;
  onCancel: () => void;
  onDone: (m: string) => void;
  onLink: (link: OneTimeLink) => void;
}) {
  const queryClient = useQueryClient();
  const emailId = useId();
  const delivery = useGetInviteDelivery({ query: { staleTime: 0 } });
  const info = delivery.isError ? UNKNOWN : delivery.data;
  const [email, setEmail] = useState("");
  const [rows, setRows] = useState<GrantRow[]>([]);
  const [how, setHow] = useState<Delivery>("email");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<InviteFailure>({});

  // Email by default when Zitadel can send it, else the link.
  useEffect(() => {
    if (info) setHow(info.email_available ? "email" : "link");
  }, [info]);
  useEffect(() => onBusy(busy), [busy, onBusy]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    const address = email.trim().toLowerCase();
    if (!looksLikeEmail(address)) {
      setFailure({ email: "Enter an email address." });
      return;
    }
    setBusy(true);
    setFailure({});
    try {
      // The plain function, not a mutation hook: a link's URL must never reach a cache.
      const result = await createInvite({ email: address, delivery: how, grants: toInviteGrants(rows) });
      refreshAccess(queryClient);
      setBusy(false);
      if (result.invite_url) {
        onLink({ email: address, url: result.invite_url });
        return;
      }
      onDone(
        result.outcome === "added"
          ? `${address} already had an account here and was added as a member. No email was sent.`
          : result.outcome === "existing_member"
            ? `${address} is already a member. Their access to the chosen projects is set.`
            : `Invited ${address}. The link in their email works for 72 hours.`,
      );
    } catch (err) {
      const f = inviteFailure(err, address, rows, projects);
      if (f.switchToEmail) setHow("email");
      setFailure(f);
      setBusy(false);
    }
  }

  const blocked = rows.some((r) => r.blocked);
  return (
    <form onSubmit={submit} noValidate className="space-y-5">
      <fieldset disabled={busy} className="m-0 min-w-0 space-y-5 border-0 p-0">
        <div className="space-y-1">
          <label htmlFor={emailId} className="text-sm font-medium">
            Email
          </label>
          <input
            id={emailId}
            type="email"
            required
            autoComplete="off"
            value={email}
            aria-invalid={failure.email ? true : undefined}
            aria-describedby={failure.email ? `${emailId}-error` : undefined}
            onChange={(e) => setEmail(e.target.value)}
            className={field}
          />
          {failure.email && (
            <p id={`${emailId}-error`} role="alert" className="text-sm text-coral">
              {failure.email}
            </p>
          )}
        </div>
        <GrantRows projects={projects} rows={rows} onChange={setRows} errors={failure.rows} />
        <DeliveryField info={info} value={how} onChange={setHow} error={failure.delivery} />
      </fieldset>
      {failure.form && (
        <p role="alert" className="text-sm text-coral">
          {failure.form}
        </p>
      )}
      <div className="flex justify-end gap-2">
        <Button type="button" variant="ghost" onClick={onCancel} disabled={busy}>
          Cancel
        </Button>
        <Button type="submit" disabled={busy || blocked || info === undefined}>
          {busy ? "Sending…" : how === "link" ? "Create link" : "Send invite"}
        </Button>
      </div>
    </form>
  );
}
