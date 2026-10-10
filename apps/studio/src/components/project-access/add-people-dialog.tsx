import { useEffect, useId, useState, type FormEvent, type RefObject } from "react";
import { useQueryClient } from "@tanstack/react-query";
import {
  createProjectInvite,
  getListProjectGrantsQueryKey,
  useGetInviteDelivery,
  type Delivery,
  type InviteDelivery,
  type Member,
  type PlatformRole,
  type ProjectGrant,
  type Project,
} from "@chelabase/management-client";
import { DeliveryField } from "@/components/members/delivery-field";
import { inviteFailure, looksLikeEmail, type InviteFailure } from "@/components/members/invite-errors";
import type { OneTimeLink } from "@/components/members/invite-link-panel";
import { refreshAccess } from "@/components/members/keys";
import { PlatformRoleField } from "@/components/members/platform-role-field";
import { RolePicker } from "@/components/members/role-picker";
import { Button } from "@/components/ui/button";
import { FreeTextCombobox } from "@/components/ui/combobox";
import { Modal } from "@/components/ui/dialog";
import { memberName, ROLE_LABEL } from "@/lib/members";
import { FailureText } from "./failure";

export interface AddPeopleDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  project: Project;
  /** Tenant members without access here: offered as suggestions. */
  suggestions: readonly Member[];
  /** Everyone in the tenant, to tell an existing member's email from a new person's. */
  members: readonly Member[];
  /** The project's grants: a member with one (or a tenant admin) can't be added again. */
  grants: readonly ProjectGrant[];
  onDone: (message: string) => void;
  onLink: (link: OneTimeLink) => void;
  finalFocus?: RefObject<HTMLElement | null>;
}

/** A lookup that failed counts as "no email": the link always works. */
const UNKNOWN: InviteDelivery = { email_available: false, reason: "zitadel_unavailable" };

/** "Add people to {project}": an email, a platform role, Postgres roles and how they get the invite. */
export function AddPeopleDialog({ open, onOpenChange, project, suggestions, members, grants, onDone, onLink, finalFocus }: AddPeopleDialogProps) {
  const [busy, setBusy] = useState(false);
  return (
    <Modal
      open={open}
      onOpenChange={(next) => !busy && onOpenChange(next)}
      title={`Add people to ${project.name}`}
      size="wide"
      finalFocus={finalFocus}
    >
      {/* Mounted only while open: closing drops the form and its errors. */}
      {open && (
        <AddForm
          project={project}
          suggestions={suggestions}
          grants={grants}
          members={members}
          onBusy={setBusy}
          onCancel={() => onOpenChange(false)}
          onDone={onDone}
          onLink={onLink}
        />
      )}
    </Modal>
  );
}

function AddForm({
  project,
  suggestions,
  grants,
  members,
  onBusy,
  onCancel,
  onDone,
  onLink,
}: Pick<AddPeopleDialogProps, "project" | "suggestions" | "members" | "grants" | "onDone" | "onLink"> & {
  onBusy: (busy: boolean) => void;
  onCancel: () => void;
}) {
  const queryClient = useQueryClient();
  const emailId = useId();
  const delivery = useGetInviteDelivery({ query: { staleTime: 0 } });
  const info = delivery.isError ? UNKNOWN : delivery.data;
  const [email, setEmail] = useState("");
  const [platformRole, setPlatformRole] = useState<PlatformRole>("developer");
  const [pgRoles, setPgRoles] = useState<string[]>([]);
  const [blocked, setBlocked] = useState(false);
  const [how, setHow] = useState<Delivery>("email");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<InviteFailure>({});

  useEffect(() => {
    if (info) setHow(info.email_available ? "email" : "link");
  }, [info]);
  useEffect(() => onBusy(busy), [busy, onBusy]);

  const address = email.trim().toLowerCase();
  // Someone already in the tenant needs no invite email: no delivery choice for them.
  const existing = members.find((m) => m.email?.toLowerCase() === address && m.status !== "removed");
  const known = existing !== undefined;
  // A grant here (or tenant admin) already exists: adding would replace it.
  const current: PlatformRole | undefined = existing
    ? existing.tenant_admin
      ? "admin"
      : grants.find((g) => g.member_id === existing.id)?.platform_role
    : undefined;

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy || current) return;
    if (!looksLikeEmail(address)) {
      setFailure({ email: "Enter an email address." });
      return;
    }
    setBusy(true);
    setFailure({});
    try {
      // The plain function, not a mutation hook: a link's URL must never reach a cache.
      const result = await createProjectInvite(project.id, { email: address, platform_role: platformRole, pg_roles: pgRoles, delivery: how });
      void queryClient.invalidateQueries({ queryKey: getListProjectGrantsQueryKey(project.id) });
      refreshAccess(queryClient);
      setBusy(false);
      if (result.invite_url) {
        onLink({ email: address, url: result.invite_url });
        return;
      }
      const role = ROLE_LABEL[platformRole];
      onDone(
        result.outcome === "existing_member"
          ? `${memberName(result.member)} now has ${role} access to ${project.name}.`
          : result.outcome === "added"
            ? `${address} already had an account and was added with ${role} access. No email was sent.`
            : `Invited ${address} with ${role} access. The link in their email works for 72 hours; access counts once they accept.`,
      );
    } catch (err) {
      const rows = [{ projectId: project.id, platformRole, pgRoles, blocked }];
      const f = inviteFailure(err, address, rows, [project]);
      if (f.switchToEmail) setHow("email");
      setFailure(f);
      setBusy(false);
    }
  }

  const rowError = failure.rows?.[project.id];
  return (
    <form onSubmit={submit} noValidate className="space-y-5">
      <fieldset disabled={busy} className="m-0 min-w-0 space-y-5 border-0 p-0">
        <div className="space-y-1">
          <FreeTextCombobox
            label="Email"
            type="email"
            required
            options={suggestions.flatMap((m) => (m.email ? [{ value: m.email, detail: memberName(m) }] : []))}
            value={email}
            onValueChange={setEmail}
            invalid={!!failure.email}
            describedBy={`${emailId}-help`}
          />
          <p id={`${emailId}-help`} className="text-xs text-chalk-dim">
            {existing
              ? "They're already here; their access is set right away."
              : "Someone new gets an email invite."}
          </p>
          {current && existing && (
            <p role="alert" className="text-sm text-coral">
              {memberName(existing)} already has {ROLE_LABEL[current]} here. Adding replaces it; edit their row instead.
            </p>
          )}
          {failure.email && (
            <p role="alert" className="text-sm text-coral">
              {failure.email}
            </p>
          )}
        </div>
        <PlatformRoleField value={platformRole} onChange={setPlatformRole} />
        <RolePicker
          databases={project.databases}
          platformRole={platformRole}
          value={pgRoles}
          onChange={setPgRoles}
          refused={rowError?.role ? new Set([rowError.role]) : undefined}
          onBlockedChange={setBlocked}
        />
        {rowError && <FailureText failure={rowError} />}
        {!known && <DeliveryField info={info} value={how} onChange={setHow} error={failure.delivery} />}
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
        <Button type="submit" disabled={busy || blocked || info === undefined || current !== undefined}>
          {busy ? "Sending…" : known ? "Add" : how === "link" ? "Create link" : "Send invite"}
        </Button>
      </div>
    </form>
  );
}
