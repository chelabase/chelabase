import { useRef, useState, type RefObject } from "react";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";

/** A one-time invite link, held only in the caller's component state. */
export interface OneTimeLink {
  email: string;
  url: string;
}

export interface InviteLinkDialogProps {
  /** The link to show; null closes the dialog (and drops the link). */
  link: OneTimeLink | null;
  title: string;
  /** Closed after the link was copied, or after "Close without copying". */
  onClose: () => void;
  finalFocus?: RefObject<HTMLElement | null>;
}

/**
 * Shows a link invite's URL, once (R35): read-only, selected on focus, a copy button and a
 * plain warning. Every way out (Done, Escape, a click outside) asks first until the link was
 * copied; the dialog never closes on its own. Nothing here stores it.
 */
export function InviteLinkDialog({ link, title, onClose, finalFocus }: InviteLinkDialogProps) {
  // The panel decides (it knows whether the link was copied); the dialog only reports attempts.
  const attempt = useRef<() => void>(() => {});
  return (
    <Modal open={link !== null} onOpenChange={(open) => !open && attempt.current()} title={title} finalFocus={finalFocus}>
      {link && <InviteLinkPanel key={link.url} link={link} onClose={onClose} attempt={attempt} />}
    </Modal>
  );
}

function InviteLinkPanel({ link, onClose, attempt }: { link: OneTimeLink; onClose: () => void; attempt: RefObject<() => void> }) {
  const [copied, setCopied] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [asking, setAsking] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(link.url);
      setCopied(true);
      setStatus("Copied.");
    } catch {
      setStatus("Copy failed: select the link and copy it.");
    }
  }
  const close = () => (copied ? onClose() : setAsking(true));
  attempt.current = close;

  return (
    <div
      className="space-y-4"
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          close();
        }
      }}
    >
      <div className="space-y-1">
        <label htmlFor="invite-link" className="text-sm font-medium">
          Invite link for {link.email}
        </label>
        <div className="flex gap-2">
          <input
            id="invite-link"
            readOnly
            value={link.url}
            autoComplete="off"
            spellCheck={false}
            onFocus={(e) => e.currentTarget.select()}
            className="h-8 min-w-0 flex-1 border border-input bg-ink px-2 font-mono text-[13px] text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
          />
          <Button type="button" variant="outline" onClick={() => void copy()}>
            Copy link
          </Button>
        </div>
        {status && (
          <p role="status" className="text-xs text-chalk-dim">
            {status}
          </p>
        )}
      </div>
      <p className="max-w-[65ch] text-sm text-chalk">
        This link is shown once. Copy it now and send it to {link.email} yourself; it works for 72 hours. Anyone with the
        link can set the password for this account.
      </p>
      {asking ? (
        <div role="alert" className="space-y-3 border border-line-strong p-3 text-sm">
          <p>You haven't copied the link. It can't be shown again.</p>
          <div className="flex justify-end gap-2">
            <Button type="button" variant="ghost" onClick={() => setAsking(false)}>
              Go back
            </Button>
            <Button type="button" variant="destructive" onClick={onClose}>
              Close without copying
            </Button>
          </div>
        </div>
      ) : (
        <div className="flex justify-end">
          <Button type="button" onClick={close}>
            Done
          </Button>
        </div>
      )}
    </div>
  );
}
