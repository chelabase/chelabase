import { useEffect, useId, useRef, useState, type ClipboardEvent, type FormEvent, type KeyboardEvent, type RefObject } from "react";
import { X } from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { createInvite, resendInvite, useGetInviteDelivery, type Delivery, type InviteDelivery, type InviteResult, type Project } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { CopyButton } from "@/components/copy-button";
import { errorText } from "@/lib/errors";
import { ROLE_LABEL } from "@/lib/members";
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

/** A lookup that failed counts as "no email": the link always works. */
const UNKNOWN: InviteDelivery = { email_available: false, reason: "zitadel_unavailable" };

/** Most addresses one batch may carry (R8). */
export const MAX_INVITES = 50;

/** "Invite people" (tenant admins): emails, initial project access (shared by the batch) and the delivery. */
export function InviteDialog({ open, onOpenChange, projects, onDone, onLink, finalFocus }: InviteDialogProps) {
  const [busy, setBusy] = useState(false);
  /** The results view renames the dialog ("Invites sent: 3 of 4"). */
  const [title, setTitle] = useState("Invite people");
  // Closed because a link was made: the link dialog has taken focus, so this one must not hand it
  // back to the trigger when its exit transition ends (that would land behind the open modal).
  const linked = useRef(false);
  // What the results view says was sent; closing it any way (Done, Escape, the close button) reports it once.
  const summary = useRef<string | null>(null);
  useEffect(() => {
    if (open) {
      linked.current = false;
      summary.current = null;
    }
    else setTitle("Invite people");
  }, [open]);
  function close(next: boolean) {
    if (!next && summary.current !== null) {
      const message = summary.current;
      summary.current = null;
      onDone(message);
    }
    onOpenChange(next);
  }
  return (
    <Modal
      open={open}
      // No closing while the request is in flight.
      onOpenChange={(next) => !busy && close(next)}
      title={title}
      size="wide"
      finalFocus={() => (linked.current ? false : (finalFocus?.current ?? true))}
    >
      {/* Mounted only while open: closing drops the form and the errors. */}
      {open && (
        <InviteForm
          projects={projects}
          onBusy={setBusy}
          onTitle={setTitle}
          onCancel={() => close(false)}
          onSummary={(m) => {
            summary.current = m;
          }}
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

/** One address in the field: the normalized address and whether it is plausible. */
interface Chip {
  email: string;
  valid: boolean;
}

/** One address's answer in a batch. */
type Sent =
  | { email: string; ok: true; outcome: InviteResult["outcome"] | "reinvited"; url?: string }
  /** `failed`: nothing was sent (retry it); `member`: an active member, whose access was set but who got no invite. */
  | { email: string; ok: false; kind: "failed" | "member"; reason: string };

const OUTCOME_WORDS: Record<string, string> = {
  invited: "Invited",
  reinvited: "Invited again with a new link; the previous one no longer works",
  added: "Added: already had an account here, no email sent",
};

const MEMBER_WORDS = "Already a member: their access is set. No invite was sent.";

/** The "Viewer on Shop, Developer on Blog" line under the results' title. */
function accessLine(rows: readonly GrantRow[], projects: readonly Project[]): string {
  if (rows.length === 0) return "No project access yet";
  return rows.map((r) => `${ROLE_LABEL[r.platformRole]} on ${projects.find((p) => p.id === r.projectId)?.name ?? "a project"}`).join(", ");
}

const chipOf = (raw: string): Chip => {
  const email = raw.trim().toLowerCase();
  return { email, valid: looksLikeEmail(email) };
};

/** Adds addresses once each (the first spelling wins). */
const withChips = (chips: readonly Chip[], more: readonly Chip[]): Chip[] => {
  const out = [...chips];
  for (const c of more) if (c.email !== "" && !out.some((x) => x.email === c.email)) out.push(c);
  return out;
};

function InviteForm({
  projects,
  onBusy,
  onTitle,
  onCancel,
  onSummary,
  onDone,
  onLink,
}: {
  projects: readonly Project[];
  onBusy: (busy: boolean) => void;
  onTitle: (title: string) => void;
  onCancel: () => void;
  onSummary: (message: string | null) => void;
  onDone: (m: string) => void;
  onLink: (link: OneTimeLink) => void;
}) {
  const queryClient = useQueryClient();
  const emailId = useId();
  const field = useRef<HTMLTextAreaElement>(null);
  const delivery = useGetInviteDelivery({ query: { staleTime: 0 } });
  const info = delivery.isError ? UNKNOWN : delivery.data;
  const [chips, setChips] = useState<Chip[]>([]);
  const [draft, setDraft] = useState("");
  const [rows, setRows] = useState<GrantRow[]>([]);
  const [how, setHow] = useState<Delivery>("email");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<InviteFailure>({});
  const [results, setResults] = useState<Sent[] | null>(null);

  // Email by default when Zitadel can send it, else the link.
  useEffect(() => {
    if (info) setHow(info.email_available ? "email" : "link");
  }, [info]);
  useEffect(() => onBusy(busy), [busy, onBusy]);
  useEffect(() => {
    onTitle(results ? `Invites sent: ${results.filter((r) => r.ok).length} of ${results.length}` : "Invite people");
  }, [results, onTitle]);
  useEffect(() => {
    if (!results) {
      onSummary(null);
      return;
    }
    const ok = results.filter((r) => r.ok).length;
    const members = results.filter((r) => !r.ok && r.kind === "member").length;
    onSummary(
      ok === results.length
        ? `Invited ${ok} ${ok === 1 ? "person" : "people"}.`
        : `Invited ${ok} of ${results.length}; ${results.length - ok - members} not sent${members > 0 ? `, ${members} already ${members === 1 ? "a member" : "members"}` : ""}.`,
    );
  }, [results, onSummary]);

  /** Text typed or pasted: every complete address (one followed by a comma, space or new line) becomes a chip. */
  function onText(text: string) {
    const parts = text.split(/[\s,;]+/);
    const rest = parts.pop() ?? "";
    const done = parts.filter((p) => p !== "").map(chipOf);
    if (done.length > 0) setChips((c) => withChips(c, done));
    setDraft(rest);
    setFailure((f) => (f.email ? { ...f, email: undefined } : f));
  }
  /** A paste is all addresses: even the last one is complete. */
  function onPaste(event: ClipboardEvent<HTMLTextAreaElement>) {
    const text = event.clipboardData.getData("text");
    if (text === "") return;
    event.preventDefault();
    setChips((c) => withChips(c, `${draft}${text}`.split(/[\s,;]+/).filter((p) => p !== "").map(chipOf)));
    setDraft("");
    setFailure((f) => (f.email ? { ...f, email: undefined } : f));
  }
  function onKey(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === "Enter") {
      // A new line closes the address; it never goes into the field.
      event.preventDefault();
      if (draft.trim() !== "") {
        setChips((c) => withChips(c, [chipOf(draft)]));
        setDraft("");
      }
    } else if (event.key === "Backspace" && draft === "" && chips.length > 0) {
      setChips((c) => c.slice(0, -1));
    }
  }

  const pending = draft.trim() === "" ? [] : [chipOf(draft)];
  const all = withChips(chips, pending);
  const valid = all.filter((c) => c.valid);
  const invalid = all.length - valid.length;
  const tooMany = all.length > MAX_INVITES;
  const batch = all.length > 1;

  async function submitOne(address: string) {
    setBusy(true);
    setFailure({});
    try {
      // The plain function, not a mutation hook: a link's URL must never reach a cache.
      const result = await createInvite({ email: address, delivery: how, grants: toInviteGrants(rows) });
      // BP-1: a person who was invited but has not accepted gets a new invite, not "already a member".
      const again = result.outcome === "existing_member" && result.member.status === "invited" && result.member.invite;
      const link = again ? (await resendInvite(again.id, { delivery: how })).invite_url : result.invite_url;
      refreshAccess(queryClient);
      setBusy(false);
      if (link) {
        onLink({ email: address, url: link });
        return;
      }
      onDone(
        again
          ? `Invited ${address} again. The link in their email works for 72 hours; the previous one no longer works.`
          : result.outcome === "added"
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

  /** R8: one request per address, in order, one at a time; a refusal doesn't stop the rest. */
  async function submitBatch(emails: readonly string[]) {
    setBusy(true);
    setFailure({});
    const sent: Sent[] = [];
    for (const email of emails) {
      try {
        const r = await createInvite({ email, delivery: how, grants: toInviteGrants(rows) });
        if (r.outcome === "existing_member") {
          if (r.member.status === "invited" && r.member.invite) {
            const out = await resendInvite(r.member.invite.id, { delivery: how });
            sent.push({ email, ok: true, outcome: "reinvited", url: out.invite_url ?? undefined });
          } else sent.push({ email, ok: false, kind: "member", reason: MEMBER_WORDS });
        } else sent.push({ email, ok: true, outcome: r.outcome, url: r.invite_url ?? undefined });
      } catch (err) {
        const f = inviteFailure(err, email, rows, projects);
        const rowMessage = f.rows ? Object.values(f.rows)[0]?.message : undefined;
        sent.push({ email, ok: false, kind: "failed", reason: f.email ?? f.delivery ?? f.form ?? rowMessage ?? errorText(err) });
      }
    }
    refreshAccess(queryClient);
    setBusy(false);
    setChips((c) => withChips(c, pending));
    setDraft("");
    setResults(sent);
  }

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || tooMany) return;
    if (all.length === 0) {
      setFailure({ email: "Enter an email address." });
      return;
    }
    if (!batch) {
      if (!all[0].valid) {
        // One address that isn't one: say so under the field and keep what was typed.
        setFailure({ email: "Enter an email address." });
        return;
      }
      await submitOne(all[0].email);
      return;
    }
    if (valid.length === 0) return;
    await submitBatch(valid.map((c) => c.email));
  }

  if (results) {
    const failed = results.filter((r) => !r.ok && r.kind === "failed");
    return (
      <div className="space-y-4">
        <p className="text-sm text-chalk-dim">{accessLine(rows, projects)}</p>
        <ul className="divide-y divide-line border-y border-line text-sm">
          {results.map((r) => (
            <li key={r.email} className="flex items-start gap-3 py-2">
              <span aria-hidden="true" className={`mt-1.5 size-2 shrink-0 ${r.ok ? "bg-[var(--seafoam)]" : r.kind === "member" ? "bg-chalk-dim" : "bg-coral"}`} />
              <div className="min-w-0 flex-1 space-y-1">
                <p className="truncate font-mono text-[13px] text-chalk">{r.email}</p>
                {r.ok ? (
                  <p className="text-chalk-dim">{OUTCOME_WORDS[r.outcome] ?? "Invited"}</p>
                ) : r.kind === "member" ? (
                  <p className="text-chalk-dim">{r.reason}</p>
                ) : (
                  <p className="text-coral">
                    Not sent: {r.reason}
                  </p>
                )}
                {r.ok && r.url && (
                  <div className="flex items-center gap-2">
                    <input
                      readOnly
                      aria-label={`Invite link for ${r.email}`}
                      value={r.url}
                      autoComplete="off"
                      spellCheck={false}
                      onFocus={(e) => e.currentTarget.select()}
                      className="h-8 min-w-0 flex-1 border border-input bg-ink px-2 font-mono text-[13px] text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand"
                    />
                    <CopyButton value={r.url} label={`Copy link for ${r.email}`} />
                  </div>
                )}
              </div>
            </li>
          ))}
        </ul>
        {results.some((r) => r.ok && r.url) && (
          <p className="max-w-[65ch] text-sm text-chalk">
            Each link is shown once. Copy them now and send each person theirs yourself; they work for 72 hours. Anyone with a link can
            set the password for that account.
          </p>
        )}
        <div className="flex justify-end gap-2">
          {failed.length > 0 && (
            <Button
              type="button"
              variant="outline"
              onClick={() => {
                setChips(failed.map((r) => ({ email: r.email, valid: true })));
                setDraft("");
                setFailure({});
                setResults(null);
              }}
            >
              Edit and retry the failed {failed.length === 1 ? "one" : "ones"}
            </Button>
          )}
          <Button
            type="button"
            onClick={() => {
              onCancel();
            }}
          >
            Done
          </Button>
        </div>
      </div>
    );
  }

  const blocked = rows.some((r) => r.blocked);
  const reasons = all.filter((c) => !c.valid && batch);
  return (
    <form onSubmit={submit} noValidate className="space-y-5">
      <fieldset disabled={busy} className="m-0 min-w-0 space-y-5 border-0 p-0">
        <div className="space-y-1">
          <label htmlFor={emailId} className="text-sm font-medium">
            Emails
          </label>
          <div className="flex min-h-8 flex-wrap items-center gap-1 border border-input bg-ink p-1 focus-within:outline-2 focus-within:-outline-offset-1 focus-within:outline-sand">
            {chips.map((c) => (
              <span
                key={c.email}
                className={`inline-flex items-center gap-1 border px-1.5 py-px font-mono text-[13px] ${c.valid ? "border-line-strong text-chalk" : "border-coral text-coral"}`}
              >
                <span>{c.email}</span>
                <button
                  type="button"
                  aria-label={`Remove ${c.email}`}
                  onClick={() => {
                    setChips((all) => all.filter((x) => x.email !== c.email));
                    field.current?.focus();
                  }}
                  className="outline-none focus-visible:outline-2 focus-visible:outline-sand"
                >
                  <X aria-hidden="true" size={12} strokeWidth={1.5} />
                </button>
              </span>
            ))}
            <textarea
              ref={field}
              id={emailId}
              rows={1}
              autoComplete="off"
              spellCheck={false}
              value={draft}
              aria-invalid={failure.email ? true : undefined}
              aria-describedby={`${emailId}-help${failure.email ? ` ${emailId}-error` : ""}`}
              onChange={(e) => onText(e.target.value)}
              onKeyDown={onKey}
              onPaste={onPaste}
              className="min-w-40 flex-1 resize-none bg-transparent px-1 py-0.5 text-sm text-chalk outline-none"
            />
          </div>
          <p id={`${emailId}-help`} className="text-xs text-chalk-dim">
            Paste or type addresses, separated by commas, spaces or new lines. Up to {MAX_INVITES}.
          </p>
          {failure.email && (
            <p id={`${emailId}-error`} role="alert" className="text-sm text-coral">
              {failure.email}
            </p>
          )}
          {reasons.map((c) => (
            <p key={c.email} className="text-sm text-coral">
              {c.email} isn't an email address. It won't be sent; fix or remove it.
            </p>
          ))}
          {tooMany && (
            <p role="alert" className="text-sm text-coral">
              Up to {MAX_INVITES} addresses at a time: remove {all.length - MAX_INVITES} to send.
            </p>
          )}
        </div>
        <DeliveryField info={info} value={how} onChange={setHow} error={failure.delivery} />
        <GrantRows projects={projects} rows={rows} onChange={setRows} errors={failure.rows} legendNote="the same for everyone in this invite" />
      </fieldset>
      {failure.form && (
        <p role="alert" className="text-sm text-coral">
          {failure.form}
        </p>
      )}
      <div className="flex flex-wrap items-center justify-end gap-2">
        {batch && (
          <p className="mr-auto text-sm text-chalk-dim tabular-nums">
            {valid.length} {valid.length === 1 ? "invite" : "invites"}
            {invalid > 0 && ` · ${invalid} to fix`}
          </p>
        )}
        <Button type="button" variant="ghost" onClick={onCancel} disabled={busy}>
          Cancel
        </Button>
        <Button type="submit" disabled={busy || blocked || info === undefined || tooMany || (batch && valid.length === 0)}>
          {busy ? (
            "Sending…"
          ) : batch ? (
            <>
              Send <span>{valid.length}</span> {valid.length === 1 ? "invite" : "invites"}
            </>
          ) : how === "link" ? (
            "Create link"
          ) : (
            "Send invite"
          )}
        </Button>
      </div>
    </form>
  );
}
