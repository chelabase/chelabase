import { useId, useState, type FormEvent } from "react";
import { Link } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import {
  ApiError,
  getBranchConnection,
  getListRolesQueryKey,
  useCreateRole,
  useDeleteGuardExemption,
  useGetMe,
  useListDatabasePersonalLogins,
  useListRoles,
  usePutGuardExemption,
  useResetRolePassword,
  type BranchRole,
  type ConnectionDetails,
} from "@chelabase/management-client";
import { plural } from "@/components/members/member-page/logins";
import { CreateRoleDialog } from "@/components/members/roles/create-role-dialog";
import { DeleteRoleDialog } from "@/components/members/roles/delete-role";
import { isManaged } from "@/components/members/roles/model";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { useProjectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { PASSWORD_STATE_TEXT } from "@/lib/password-state";
import { CopyButton } from "./copy-button";

export interface BranchRolesProps {
  /** The project the branch belongs to: "Manage in Members" opens the hub on it. */
  projectId: string;
  databaseId: string;
  branchId: string;
}

type Pending = { kind: "reset"; role: BranchRole } | null;
type Shown = { role: string; details: ConnectionDetails; fresh: boolean; notice: string | null };

/** The API's name rule; reserved names are left to the API. */
const NAME_RULE = /^[a-z_][a-z0-9_]{0,62}$/;
const NOT_APPLIED_NOTICE = "The new password is stored but not yet applied; it will be retried.";
const ORIGIN_LABEL: Record<BranchRole["origin"], string> = { sql: "SQL", api: "Studio", owner: "Owner", personal: "Personal" };
const utc = (iso: string) => `${new Date(iso).toISOString().slice(0, 16).replace("T", " ")} UTC`;
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
/** `d MMM yyyy`, in UTC. */
const created = (iso: string) => {
  const d = new Date(iso);
  return `${d.getUTCDate()} ${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()}`;
};

/**
 * The attributes beyond login and level, read-only (they're set through the API or a merge):
 * only those that differ from a new role's defaults are named.
 */
function RoleAttributes({ role }: { role: BranchRole }) {
  const parts: { text: string; hazard?: boolean }[] = [];
  if (role.bypass_rls) parts.push({ text: "Bypasses RLS", hazard: true });
  if (role.inherit === false) parts.push({ text: "No inherit" });
  if (typeof role.connection_limit === "number" && role.connection_limit >= 0) {
    parts.push({ text: `At most ${role.connection_limit} connection${role.connection_limit === 1 ? "" : "s"}` });
  }
  if (role.valid_until) parts.push({ text: `Valid until ${utc(role.valid_until)}` });
  if (role.member_of?.length) parts.push({ text: `Member of ${role.member_of.join(", ")}` });
  if (parts.length === 0) return <span className="text-muted-foreground">—</span>;
  return (
    <ul className="space-y-0.5 text-xs">
      {parts.map((p) => (
        <li key={p.text} className={p.hazard ? "text-destructive" : undefined}>
          {p.text}
        </li>
      ))}
    </ul>
  );
}

/**
 * The branch's roles for the people who run it (Part 4): the managed roles and the owner, never a
 * member's personal login (R3: those are counted, and managed in the Members hub). Plain roles are
 * created and deleted through the hub's dialogs; owner-level creation stays here.
 */
export function BranchRoles({ projectId, databaseId, branchId }: BranchRolesProps) {
  // Revealing, resetting, creating and deleting roles are Admin's; others read the list.
  const admin = useProjectCaps().atLeast("admin");
  const queryClient = useQueryClient();
  const roles = useListRoles(databaseId, branchId, {
    query: { refetchInterval: (q) => (q.state.data?.some((r) => r.pending) ? 2000 : false) },
  });
  const refresh = () => queryClient.invalidateQueries({ queryKey: getListRolesQueryKey(databaseId, branchId) });
  // On settle, not just success: a failed call may still have changed the roles (a create
  // or delete that's stored but not applied, a create whose role was demoted).
  const create = useCreateRole({ mutation: { onSettled: refresh } });
  const reset = useResetRolePassword({ mutation: { onSettled: refresh } });
  const exempt = usePutGuardExemption({ mutation: { onSettled: refresh } });
  const unexempt = useDeleteGuardExemption({ mutation: { onSettled: refresh } });
  const me = useGetMe();
  // P24: Admins see how many personal logins the database has; the read is Admin-only, so nobody else asks.
  const logins = useListDatabasePersonalLogins(databaseId, { query: { enabled: admin } });
  const activeLogins = (logins.data?.logins ?? []).filter((l) => l.status === "active").length;
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<string | null>(null);
  const [shown, setShown] = useState<Shown | null>(null);
  const [pending, setPending] = useState<Pending>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // Set by a submit with a bad name; the hint shows until the name changes.
  const [nameHint, setNameHint] = useState<string | null>(null);
  const nameId = useId();
  const hintId = useId();
  const all = roles.data ?? [];
  const rows = all.filter((r) => r.origin !== "personal");
  const project = me.data?.tenant?.projects.find((p) => p.id === projectId);
  const where = project ? [project.name, project.databases.find((d) => d.database_id === databaseId)?.name].filter(Boolean).join(" · ") : "this branch";
  const hub = { tab: "roles", project: projectId, database: databaseId, branch: branchId } as const;
  const nameReason =
    name.trim() === ""
      ? "Enter a name for the role."
      : NAME_RULE.test(name.trim())
        ? null
        : "Role names use lowercase letters, digits and underscores, start with a letter or an underscore, and have at most 63 characters.";

  /** Passwords stay out of the query cache: the plain function, into component state. */
  async function reveal(role: BranchRole | string, fresh: boolean, notice: string | null = null) {
    const roleName = typeof role === "string" ? role : role.name;
    const isOwner = typeof role !== "string" && role.origin === "owner";
    const details = await getBranchConnection(databaseId, branchId, isOwner ? undefined : { role: roleName });
    setShown({ role: roleName, details, fresh, notice });
  }

  async function guarded(action: () => Promise<void>) {
    setBusy(true);
    setError(null);
    try {
      await action();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  function setExempt(role: BranchRole, on: boolean) {
    void guarded(async () => {
      await (on ? exempt : unexempt).mutateAsync({ id: databaseId, branchId, role: role.name });
    });
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    if (nameReason) {
      setNameHint(nameReason);
      return;
    }
    void guarded(async () => {
      const made = await create.mutateAsync({ id: databaseId, branchId, data: { name: name.trim(), owner_level: true } });
      setName("");
      setShown(null);
      await reveal(made.name, true);
    });
  }

  function confirm() {
    if (!pending) return;
    const { role } = pending;
    void guarded(async () => {
      setPending(null);
      setShown(null);
      try {
        await reset.mutateAsync({ id: databaseId, branchId, role: role.name });
      } catch (err) {
        if (!(err instanceof ApiError && err.code === "not_applied")) throw err;
        await reveal(role, false, NOT_APPLIED_NOTICE);
        return;
      }
      await reveal(role, false, "The password was reset. Update anything using the old one.");
    });
  }

  const field = "h-8 border border-input bg-background px-2 text-sm";
  const badge = "whitespace-nowrap border px-2 py-0.5 text-xs";
  return (
    <div className="space-y-2 border p-3 text-sm">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        <h3 className="mr-auto font-medium">Roles</h3>
        {admin && activeLogins > 0 && (
          <Link to="/app/members" search={hub} className="text-muted-foreground underline underline-offset-4">
            {plural(activeLogins, "personal login")}
          </Link>
        )}
        <Link to="/app/members" search={hub} className="underline underline-offset-4">
          Manage in Members
        </Link>
        {admin && (
          <Button size="sm" disabled={busy} onClick={() => setCreating(true)}>
            Create role
          </Button>
        )}
      </div>
      {roles.isError && (
        <p role="alert" className="text-destructive">
          {errorText(roles.error)}
        </p>
      )}
      <div className="relative max-w-full overflow-x-auto">
      <table className="w-full min-w-[40rem] text-left">
        <thead className="border-b text-muted-foreground">
          <tr>
            <th className="py-1 pr-4 font-medium">Name</th>
            <th className="py-1 pr-4 font-medium">Origin</th>
            <th className="py-1 pr-4 font-medium">Level</th>
            <th className="py-1 pr-4 font-medium">Login</th>
            <th className="whitespace-nowrap py-1 pr-4 font-medium">Attributes</th>
            <th className="whitespace-nowrap py-1 pr-4 font-medium">Login guard</th>
            <th className="py-1 pr-4 font-medium">Password</th>
            <th className="py-1 pr-4 font-medium">Created</th>
            <th className="py-1 font-medium">Actions</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.name} className="border-b last:border-0">
              <td className="py-1 pr-4 font-mono">
                {isManaged(r) ? (
                  <Link
                    to="/app/members/roles/$databaseId/$branchId/$role"
                    params={{ databaseId, branchId, role: r.name }}
                    className="underline decoration-muted-foreground underline-offset-4"
                  >
                    {r.name}
                  </Link>
                ) : (
                  r.name
                )}
              </td>
              <td className="py-1 pr-4">
                <span className={badge}>{ORIGIN_LABEL[r.origin]}</span>
              </td>
              <td className="py-1 pr-4">
                <span className="inline-flex gap-1 whitespace-nowrap">
                  <span className={badge}>{r.level === "owner_level" ? "Owner-level" : "Plain"}</span>
                  {r.pending && <span className={badge}>Pending</span>}
                </span>
              </td>
              <td className="py-1 pr-4">{r.login ? "Yes" : "No"}</td>
              <td className="py-1 pr-4">
                <RoleAttributes role={r} />
              </td>
              <td className="py-1 pr-4">
                {r.login && r.origin !== "owner" && (
                  <Switch
                    aria-label={`Exempt ${r.name} from the login guard`}
                    checked={r.guard_exempt}
                    disabled={busy || !admin}
                    onCheckedChange={(on) => setExempt(r, on)}
                  />
                )}
              </td>
              <td className="py-1 pr-4">
                <span className={r.password === "generated" ? "whitespace-nowrap" : "whitespace-nowrap text-xs text-destructive"}>
                  {PASSWORD_STATE_TEXT[r.password]}
                </span>
                {r.blocked_keys.length > 0 && (
                  <span className="block text-xs text-muted-foreground">
                    Data API keys blocked: {r.blocked_keys.map((k) => k.name).join(", ")}
                  </span>
                )}
              </td>
              <td className="whitespace-nowrap py-1 pr-4">{created(r.created_at)}</td>
              <td className="py-1">
                {admin && (
                <div className="flex gap-2">
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={busy}
                    aria-label={`Reveal ${r.name}`}
                    onClick={() => void guarded(() => reveal(r, false))}
                  >
                    Reveal
                  </Button>
                  <Button
                    size="xs"
                    variant="outline"
                    disabled={busy}
                    aria-label={`Reset password for ${r.name}`}
                    onClick={() => setPending({ kind: "reset", role: r })}
                  >
                    Reset password
                  </Button>
                  {r.origin !== "owner" && (
                    <Button
                      size="xs"
                      variant="outline"
                      disabled={busy}
                      aria-label={`Delete ${r.name}`}
                      onClick={() => setDeleting(r.name)}
                    >
                      Delete
                    </Button>
                  )}
                </div>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      </div>

      {pending && (
        <div className="space-y-2 border p-3">
          <p>
            {`Reset the password for ${pending.role.name}? The current password stops working for new connections.`}
          </p>
          <div className="flex gap-2">
            <Button size="sm" disabled={busy} onClick={confirm}>
              Confirm reset
            </Button>
            <Button size="sm" variant="outline" onClick={() => setPending(null)}>
              Cancel
            </Button>
          </div>
        </div>
      )}

      {shown && (
        <section aria-label={`Connection for ${shown.role}`} className="space-y-2 border p-3">
          {shown.fresh && <p>Copy this password now. You can reveal it again later.</p>}
          {shown.notice && (
            <p role="status" className="text-muted-foreground">
              {shown.notice}
            </p>
          )}
          <dl className="grid grid-cols-[auto_1fr_auto] items-center gap-x-4 gap-y-1">
            <dt className="text-muted-foreground">User</dt>
            <dd className="font-mono break-all">{shown.details.user}</dd>
            <dd>
              <CopyButton value={shown.details.user} label="Copy user" />
            </dd>
            <dt className="text-muted-foreground">Password</dt>
            {shown.details.password === null ? (
              <dd className="col-span-2">{PASSWORD_STATE_TEXT[shown.details.password_state]}</dd>
            ) : (
              <>
                <dd className="font-mono break-all">{shown.details.password}</dd>
                <dd>
                  <CopyButton value={shown.details.password} label="Copy password" />
                </dd>
              </>
            )}
            <dt className="text-muted-foreground">URI</dt>
            <dd className="font-mono break-all">{shown.details.uri}</dd>
            <dd>
              <CopyButton value={shown.details.uri} label="Copy URI" />
            </dd>
          </dl>
          {shown.details.ca_file && (
            <p className="text-xs text-muted-foreground">
              Local dev: pass the proxy&apos;s CA with <code>sslrootcert={shown.details.ca_file}</code>.
            </p>
          )}
          <Button size="sm" variant="outline" onClick={() => setShown(null)}>
            Dismiss
          </Button>
        </section>
      )}

      {admin && (
        <form onSubmit={submit} className="space-y-1">
          <div className="flex flex-wrap items-end gap-3">
            <div className="flex flex-col gap-1">
              <label htmlFor={nameId}>Owner-level role name</label>
              <input
                id={nameId}
                value={name}
                onChange={(e) => {
                  setName(e.target.value);
                  setNameHint(null);
                }}
                maxLength={63}
                aria-invalid={nameHint ? true : undefined}
                aria-describedby={nameHint ? hintId : undefined}
                className={`${field} w-48`}
              />
            </div>
            <Button type="submit" size="sm" variant="outline" disabled={busy}>
              Create owner-level role
            </Button>
          </div>
          <p className="text-xs text-muted-foreground">An owner-level role bypasses RLS and can create roles. Make plain roles with Create role.</p>
        </form>
      )}
      {nameHint && (
        <p id={hintId} className="text-xs text-destructive">
          {nameHint}
        </p>
      )}
      {error && (
        <p role="alert" className="text-destructive">
          {error}
        </p>
      )}
      {admin && (
        <CreateRoleDialog
          open={creating}
          onOpenChange={setCreating}
          databaseId={databaseId}
          branchId={branchId}
          where={where}
          roles={all}
          onCreated={() => {
            setCreating(false);
            void refresh();
          }}
        />
      )}
      {deleting !== null && (
        <DeleteRoleDialog
          open
          onOpenChange={(open) => {
            if (open) return;
            setDeleting(null);
            // The delete may have landed even when it reported a failure.
            void refresh();
          }}
          databaseId={databaseId}
          branchId={branchId}
          role={deleting}
          roles={all}
          onDeleted={() => setShown((s) => (s?.role === deleting ? null : s))}
        />
      )}
    </div>
  );
}
