import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { KeyRound, MoreHorizontal } from "lucide-react";
import {
  ApiError,
  getListMemberPersonalLoginsQueryKey,
  reinstateMember,
  removeMember,
  resendInvite,
  revokeInvite,
  useGetInviteDelivery,
  revokeMemberPostgresAccess,
  suspendMember,
  updateMember,
  useListMemberPersonalLogins,
  useListTeams,
  type Member,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Menu, MenuItem } from "@/components/ui/menu";
import { errorText } from "@/lib/errors";
import { refusalOrErrorText } from "@/lib/refusal-extra";
import { RefusalText } from "@/components/project-access/refusal-text";
import { memberName } from "@/lib/members";
import { escapeName } from "../hub";
import { InviteLinkDialog, type OneTimeLink } from "../invite-link-panel";
import { copyFor, LAST_ADMIN } from "../people-tab";
import { refreshAccess } from "../keys";
import { firstName, plural } from "./logins";

type Ask =
  | { kind: "suspend" }
  | { kind: "reinstate" }
  | { kind: "revoke" }
  | { kind: "revoke-invite" }
  | { kind: "remove-admin" }
  | { kind: "make-admin" | "step-down" | "remove" };

export interface MemberActionsProps {
  member: Member;
  /** The caller is a tenant admin: only they get these (Part 7). */
  tenantAdmin: boolean;
  /** The page is the caller's own. */
  self: boolean;
  /** Other tenant admins exist, so stepping down leaves someone. */
  otherAdmins: boolean;
  /** What was just done, said once on the page. */
  onStatus: (message: string | null) => void;
}

/**
 * The header's actions, by power. Suspend, Reinstate and Revoke Postgres access now are tenant-admin
 * only; each asks first and keeps a refusal in its dialog. The copy never guesses a pronoun from a name.
 */
export function MemberActions({ member, tenantAdmin, self, otherAdmins, onStatus }: MemberActionsProps) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const logins = useListMemberPersonalLogins(member.id, { query: { enabled: tenantAdmin } });
  const [ask, setAsk] = useState<Ask | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const teamList = useListTeams({ query: { enabled: tenantAdmin } });
  const invite = member.status === "invited" ? member.invite : null;
  // Only tenant admins act on invites, so only they ask whether email can be sent.
  const delivery = useGetInviteDelivery({ query: { enabled: tenantAdmin && invite != null } });
  const [link, setLink] = useState<{ link: OneTimeLink; title: string } | null>(null);
  const [inviteBusy, setInviteBusy] = useState(false);
  const [inviteError, setInviteError] = useState<string | null>(null);
  if (!tenantAdmin) return null;

  const name = memberName(member);
  const first = firstName(name);
  const active = member.status === "active";
  const suspended = member.status === "suspended";

  const open = (a: Ask) => {
    setError(null);
    setAsk(a);
  };
  const refresh = () => {
    refreshAccess(queryClient);
    void queryClient.invalidateQueries({ queryKey: getListMemberPersonalLoginsQueryKey(member.id) });
  };

  /** The plain function: a new link's URL must never reach a cache. */
  async function resend(how: "email" | "link") {
    if (!invite) return;
    const email = member.email ?? name;
    setInviteBusy(true);
    setInviteError(null);
    onStatus(null);
    try {
      const out = await resendInvite(invite.id, { delivery: how });
      if (out.invite_url) setLink({ link: { email, url: out.invite_url }, title: `New invite link for ${email}` });
      else onStatus(`Sent a new invite to ${email}. The previous link no longer works.`);
    } catch (err) {
      if (err instanceof ApiError && err.code === "invite_closed") setInviteError("This invite was revoked or accepted in the meantime.");
      else if (err instanceof ApiError && err.code === "link_not_allowed") {
        setInviteError(`${email} already has a way to sign in, so a link can't be used. Resend the invite by email.`);
      } else setInviteError(errorText(err));
    } finally {
      setInviteBusy(false);
      refresh();
    }
  }

  async function run(a: Ask) {
    setBusy(true);
    setError(null);
    try {
      switch (a.kind) {
        case "suspend":
          await suspendMember(member.id);
          onStatus(`${name} is suspended.`);
          break;
        case "reinstate":
          await reinstateMember(member.id);
          onStatus(`${name} is reinstated. ${first} makes new personal logins.`);
          break;
        case "revoke": {
          const out = await revokeMemberPostgresAccess(member.id);
          onStatus(
            `Revoked ${plural(out.revoked, "login")} and ended every Postgres session of ${name}; passwords rotated on ${plural(out.rotated_branches, "branch", "branches")}.`,
          );
          break;
        }
        case "revoke-invite":
          if (!invite) return;
          await revokeInvite(invite.id);
          refresh();
          setAsk(null);
          void navigate({ to: "/app/members" });
          return;
        case "make-admin":
          await updateMember(member.id, { tenant_admin: true });
          onStatus(`${name} is now a tenant admin.`);
          break;
        case "remove-admin":
          await updateMember(member.id, { tenant_admin: false });
          onStatus(`${name} is no longer a tenant admin.`);
          break;
        case "step-down":
          await updateMember(member.id, { tenant_admin: false });
          onStatus("You stepped down as tenant admin.");
          break;
        case "remove":
          await removeMember(member.id);
          refresh();
          setAsk(null);
          void navigate({ to: "/app/members" });
          return;
      }
      refresh();
      setAsk(null);
    } catch (err) {
      setError(err instanceof ApiError && err.code === "last_tenant_admin" && a.kind !== "suspend" ? LAST_ADMIN(name) : refusalOrErrorText(err, { teamName: (id) => (teamList.data ?? []).find((t) => t.id === id)?.name }));
    } finally {
      setBusy(false);
    }
  }

  const live = (logins.data?.logins ?? []).filter((l) => l.status === "active");

  return (
    <>
      <div className="flex flex-wrap items-center gap-2">
        {active && !member.tenant_admin && !self && (
          <Button variant="outline" onClick={() => open({ kind: "make-admin" })}>
            Make tenant admin
          </Button>
        )}
        {active && member.tenant_admin && !self && (
          <Button variant="outline" onClick={() => open({ kind: "remove-admin" })}>
            Remove tenant admin
          </Button>
        )}
        {active && member.tenant_admin && self && otherAdmins && (
          <Button variant="outline" onClick={() => open({ kind: "step-down" })}>
            Step down as tenant admin
          </Button>
        )}
        {active && (
          <Button variant="outline" onClick={() => open({ kind: "revoke" })}>
            <KeyRound aria-hidden="true" size={16} strokeWidth={1.5} />
            Revoke Postgres access now
          </Button>
        )}
        {active && !self && (
          <Button variant="destructive" onClick={() => open({ kind: "suspend" })}>
            Suspend
          </Button>
        )}
        {suspended && <Button onClick={() => open({ kind: "reinstate" })}>Reinstate</Button>}
        {invite && delivery.data?.email_available === true && (
          <Button variant="outline" disabled={inviteBusy} onClick={() => void resend("email")}>
            Resend invite
          </Button>
        )}
        {invite && (
          <>
            <Button variant="outline" disabled={inviteBusy} onClick={() => void resend("link")}>
              New invite link
            </Button>
            <Button variant="destructive" disabled={inviteBusy} onClick={() => open({ kind: "revoke-invite" })}>
              Revoke invite
            </Button>
          </>
        )}
        {(active || suspended) && !self && (
          <Menu
            trigger={
              <Button variant="outline" size="icon" aria-label={`More actions for ${name}`}>
                <MoreHorizontal aria-hidden="true" size={18} strokeWidth={1.5} />
              </Button>
            }
          >
            <MenuItem destructive onClick={() => open({ kind: "remove" })}>
              Remove from tenant
            </MenuItem>
          </Menu>
        )}
      </div>
      {inviteError && (
        <p role="alert" className="basis-full text-sm text-coral">
          {inviteError}
        </p>
      )}
      <InviteLinkDialog
        link={link?.link ?? null}
        title={link?.title ?? ""}
        onClose={() => {
          if (link) onStatus(`Created a new invite link for ${link.link.email}. The previous link no longer works.`);
          setLink(null);
        }}
      />
      {ask?.kind === "revoke-invite" && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setAsk(null)}
          {...copyFor({ kind: "revoke", member })}
          busy={busy}
          error={error && <RefusalText text={error} />}
          onConfirm={() => void run(ask)}
        />
      )}

      {ask?.kind === "suspend" && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setAsk(null)}
          title={`Suspend ${name}?`}
          body={
            <>
              <p>
                {first} is signed out of Studio and can't sign in. {first}'s personal Postgres logins stop working at once and every database session{" "}
                {first} has open ends.
              </p>
              <p className="text-chalk-dim">
                {first}'s project access and roles are kept, so Reinstate gives them back. {first} makes new personal logins afterwards. To take everything away for
                good, remove {first} from the tenant instead.
              </p>
            </>
          }
          confirm={`Suspend ${first}`}
          tone="destructive"
          busy={busy}
          error={error && <RefusalText text={error} />}
          onConfirm={() => void run(ask)}
        />
      )}
      {ask?.kind === "reinstate" && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setAsk(null)}
          title={`Reinstate ${name}?`}
          body={
            <p>
              {first} can sign in again with the access listed on this page. Personal logins aren't restored; {first} creates new ones.
            </p>
          }
          confirm="Reinstate"
          busy={busy}
          error={error && <RefusalText text={error} />}
          onConfirm={() => void run(ask)}
        />
      )}
      {ask?.kind === "revoke" && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setAsk(null)}
          title={`Revoke ${first}'s Postgres access now?`}
          body={
            <>
              <p>For a password that may have leaked. At once:</p>
              <ul className="list-disc space-y-1 pl-5">
                {live.map((l) => (
                  <li key={l.database_id}>
                    <span className="font-mono text-[13px]">{escapeName(l.role_name)}</span> on {l.database_name} stops working
                  </li>
                ))}
                <li>
                  every Postgres session they have open ends, including their SQL editor and table editor sessions, which reconnect with new passwords.
                </li>
              </ul>
              {live.length === 0 && logins.data && <p className="text-chalk-dim">{first} has no personal logins to revoke.</p>}
              <p className="text-chalk-dim">
                {first}'s Studio access and project roles don't change, and {first} can create new personal logins.
              </p>
            </>
          }
          confirm="Revoke access now"
          tone="destructive"
          busy={busy}
          error={error && <RefusalText text={error} />}
          onConfirm={() => void run(ask)}
        />
      )}
      {ask?.kind === "remove-admin" && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setAsk(null)}
          title={`Remove ${name} as tenant admin?`}
          body={<p>{name} stays a member and keeps their project access, but can no longer invite or remove people, or reach projects they have no grant on.</p>}
          confirm="Remove tenant admin"
          tone="destructive"
          busy={busy}
          error={error && <RefusalText text={error} />}
          onConfirm={() => void run(ask)}
        />
      )}
      {(ask?.kind === "make-admin" || ask?.kind === "step-down" || ask?.kind === "remove") && (
        <ConfirmDialog
          open
          onOpenChange={(next) => !next && setAsk(null)}
          {...copyFor({ kind: ask.kind, member })}
          busy={busy}
          error={error && <RefusalText text={error} />}
          onConfirm={() => void run(ask)}
        />
      )}
    </>
  );
}
