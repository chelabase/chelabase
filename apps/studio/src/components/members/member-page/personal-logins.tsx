import { Select } from "@/components/ui/select";
import { useEffect, useId, useState, type FormEvent } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Check, KeyRound, MoreHorizontal, Plus, RefreshCw } from "lucide-react";
import {
  ApiError,
  createPersonalLogin,
  deletePersonalLogin,
  getListMemberPersonalLoginsQueryKey,
  resetPersonalLoginPassword,
  useListMemberPersonalLogins,
  type DatabaseSummary,
  type MeResponse,
  type Member,
  type PersonalLoginSecret,
  type PersonalLoginView,
  type Project,
} from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { Modal } from "@/components/ui/dialog";
import { Menu, MenuItem, MenuNote } from "@/components/ui/menu";
import { projectCaps } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { memberName, utc } from "@/lib/members";
import { cn } from "@/lib/utils";
import { escapeName } from "../hub";
import { useGroupRoles } from "./default-role";
import {
  dateText,
  daysFrom,
  defaultLoginName,
  firstName,
  LOGIN_NAME,
  LOGIN_NAME_RULE,
  passwordlessString,
  phaseOf,
  REVOKE_REASON,
  withDefaultRole,
  type LoginPhase,
} from "./logins";
import { RevealDialog } from "./reveal-dialog";

const field =
  "h-8 min-w-0 border border-input bg-ink px-2 text-sm text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand";

interface Target {
  project: Project;
  /** Ready databases where the caller holds Developer or more. */
  databases: DatabaseSummary[];
}

/** The projects and databases a member may log in to (Developer or more, MH-17; R9: not a Viewer). */
function targetsFor(me: MeResponse): Target[] {
  return (me.tenant?.projects ?? [])
    .map((project) => ({
      project,
      // A narrowing only lowers a grant: both the project's role and the database's must reach Developer.
      databases: project.databases.filter(
        (d) => d.status === "ready" && projectCaps(me, project.id).atLeast("developer") && projectCaps(me, project.id, d.database_id).atLeast("developer"),
      ),
    }))
    .filter((t) => t.databases.length > 0);
}

export interface PersonalLoginsProps {
  member: Member;
  me: MeResponse;
  /** The page is the caller's own: they create, reset and renew. */
  self: boolean;
}

/**
 * Part 6's list on the member page. The owner of the page creates, resets, renews and deletes; an
 * Admin only deletes. A created or reset password goes to the reveal dialog's state and nowhere else
 * (MH-38c): these calls are plain functions, never a mutation, so no cache keeps the answer.
 */
export function PersonalLogins({ member, me, self }: PersonalLoginsProps) {
  const queryClient = useQueryClient();
  const logins = useListMemberPersonalLogins(member.id);
  const [creating, setCreating] = useState(false);
  const [reveal, setReveal] = useState<{ database: string; request: () => Promise<PersonalLoginSecret> } | null>(null);
  const [deleting, setDeleting] = useState<PersonalLoginView | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const name = memberName(member);
  const first = firstName(name);
  const targets = self ? targetsFor(me) : [];
  const rows = logins.data?.logins ?? [];
  const refresh = () => void queryClient.invalidateQueries({ queryKey: getListMemberPersonalLoginsQueryKey(member.id) });
  const projectOf = (id: string) => me.tenant?.projects.find((p) => p.id === id);

  /** Starts the reveal dialog, which asks the server itself and is the only holder of the password. */
  function startReveal(database: string, request: () => Promise<PersonalLoginSecret>) {
    setBusy(database);
    setActionError(null);
    setReveal({ database, request });
  }
  const reset = (l: PersonalLoginView) => startReveal(l.database_id, () => resetPersonalLoginPassword(l.database_id));

  const soon = self ? rows.filter((l) => phaseOf(l) === "soon" || phaseOf(l) === "expired") : [];

  return (
    <section aria-labelledby="personal-logins" className="space-y-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div className="space-y-1">
          <h2 id="personal-logins" className="text-base font-semibold">
            Personal Postgres logins
          </h2>
          <p className="max-w-[65ch] text-sm text-chalk-dim">
            {self ? "Your own Postgres login on a database, to connect with psql as yourself." : `${first}'s own Postgres logins, one per database.`}
          </p>
        </div>
        {self && targets.length > 0 && (
          <Button variant="outline" onClick={() => setCreating(true)}>
            <Plus aria-hidden="true" size={16} strokeWidth={1.5} />
            New login
          </Button>
        )}
      </div>

      {logins.isPending ? (
        <p className="text-sm text-chalk-dim">Loading logins…</p>
      ) : logins.isError && !logins.data ? (
        <div className="space-y-3">
          <p role="alert" className="text-sm text-coral">
            Couldn't load logins: {errorText(logins.error)}
          </p>
          <Button variant="outline" onClick={() => void logins.refetch()}>
            Retry
          </Button>
        </div>
      ) : rows.length === 0 ? (
        <p className="max-w-[65ch] text-sm text-chalk-dim">
          {self
            ? targets.length > 0
              ? "No personal logins yet. Create one to connect with psql as yourself."
              : "No personal logins yet. A login needs Developer access or more on a database."
            : "No personal logins yet."}
        </p>
      ) : (
        <div className="relative overflow-x-auto">
          <table aria-label="Personal Postgres logins" className="w-full min-w-[30rem] border-y border-line text-sm">
            <thead>
              <tr className="border-b border-line bg-channel text-left text-xs text-chalk-dim">
                <th scope="col" className="py-2 pr-3 font-normal">
                  Database
                </th>
                <th scope="col" className="py-2 pr-3 font-normal">
                  Login
                </th>
                <th scope="col" className="py-2 pr-3 font-normal">
                  Expires
                </th>
                <th scope="col" className="py-2 pr-3 font-normal max-sm:hidden">
                  Created
                </th>
                <th scope="col" className="py-2 font-normal">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {rows.map((l) => (
                <LoginRow
                  key={l.database_id}
                  login={l}
                  me={me}
                  project={projectOf(l.project_id)}
                  self={self}
                  busy={busy === l.database_id}
                  onReset={() => void reset(l)}
                  onDelete={() => setDeleting(l)}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}

      {actionError && (
        <p role="alert" className="text-sm text-coral">
          {actionError}
        </p>
      )}

      {soon.map((l) => (
        <p key={l.database_id} className="flex max-w-[65ch] gap-2 border border-line bg-deep p-3 text-sm text-chalk">
          <RefreshCw aria-hidden="true" size={16} strokeWidth={1.5} className="mt-0.5 shrink-0 text-chalk-dim" />
          <span>
            {phaseOf(l) === "expired"
              ? `${l.role_name} on ${l.database_name} expired on ${dateText(l.valid_until)}.`
              : `${l.role_name} on ${l.database_name} expires on ${dateText(l.valid_until)}.`}{" "}
            Renew it for 90 more days: you get a new password, and the old one stops working.
          </span>
        </p>
      ))}

      {rows.length > 0 || self ? (
        <div className="max-w-[65ch] space-y-1 text-xs text-chalk-dim">
          <p>
            A login works on every branch of its database where {self ? "you have" : `${first} has`} Developer access or more. Connection strings are for the
            main branch; on another branch, use that branch's host.
          </p>
          {!self && <p>Only {first} can create, reset or renew their logins.</p>}
        </div>
      ) : null}

      {creating && (
        <NewLoginDialog
          member={member}
          me={me}
          targets={targets}
          onClose={() => setCreating(false)}
          onCreated={refresh}
        />
      )}
      {reveal && (
        <RevealDialog
          request={reveal.request}
          me={me}
          onLoaded={() => {
            setBusy(null);
            refresh();
          }}
          onFailed={(err) => {
            setActionError(errorText(err));
            setBusy(null);
            setReveal(null);
          }}
          onClose={() => setReveal(null)}
        />
      )}
      {deleting && (
        <DeleteLogin
          login={deleting}
          memberId={member.id}
          who={self ? "You" : first}
          onClose={() => setDeleting(null)}
          onDeleted={() => {
            setDeleting(null);
            refresh();
          }}
        />
      )}
    </section>
  );
}

function expiryText(l: PersonalLoginView, phase: LoginPhase): string {
  if (phase === "revoked") {
    const reason = l.revoked_reason ? REVOKE_REASON[l.revoked_reason] : null;
    return `Revoked${l.revoked_at ? ` ${dateText(l.revoked_at)}` : ""}${reason ? `: ${reason}` : ""}`;
  }
  return `${phase === "expired" ? "Expired " : ""}${daysFrom(l.valid_until)} · ${dateText(l.valid_until)}`;
}

interface LoginRowProps {
  login: PersonalLoginView;
  me: MeResponse;
  project: Project | undefined;
  self: boolean;
  busy: boolean;
  onReset: () => void;
  onDelete: () => void;
}

function LoginRow({ login, me, project, self, busy, onReset, onDelete }: LoginRowProps) {
  const phase = phaseOf(login);
  const database = project?.databases.find((d) => d.database_id === login.database_id);
  const groups = useGroupRoles(me, login.project_id, database);
  const [role, setRole] = useState("");
  // "Copied." / "Copy failed", said beside the menu for a moment.
  const [copied, setCopied] = useState<string | null>(null);
  useEffect(() => {
    if (copied === null) return;
    const t = setTimeout(() => setCopied(null), 3000);
    return () => clearTimeout(t);
  }, [copied]);
  async function copy(value: string) {
    try {
      await navigator.clipboard.writeText(value);
      setCopied("Copied.");
    } catch {
      setCopied("Copy failed: select and copy it by hand.");
    }
  }
  const base = self && (phase === "ok" || phase === "soon") ? passwordlessString(login, database) : null;
  const dim = phase === "revoked" ? "text-chalk-dim" : "text-chalk";
  const loginName = escapeName(login.role_name);

  return (
    <tr className={cn("border-b border-line align-top last:border-b-0", dim)}>
      <td className="py-2 pr-3">
        <div>{login.database_name}</div>
        {project && <div className="text-xs text-chalk-dim">{project.name}</div>}
      </td>
      <td className="py-2 pr-3 font-mono text-[13px]">{loginName}</td>
      <td className="py-2 pr-3">
        <span className="inline-flex items-center gap-2" title={utc(login.valid_until)}>
          {phase === "soon" && <span aria-hidden="true" className="inline-block size-2 border border-coral" />}
          <span className={cn(phase === "expired" && "text-coral", phase === "revoked" && "text-chalk-dim")}>{expiryText(login, phase)}</span>
        </span>
      </td>
      <td className="py-2 pr-3 text-chalk-dim max-sm:hidden">{dateText(login.created_at)}</td>
      <td className="py-2">
        <div className="flex flex-wrap items-center justify-end gap-1">
          {self && (phase === "soon" || phase === "expired") && (
            <Button variant="outline" size="sm" disabled={busy} onClick={onReset}>
              <RefreshCw aria-hidden="true" size={14} strokeWidth={1.5} />
              Renew
            </Button>
          )}
          {self && phase === "ok" && (
            <Button variant="outline" size="sm" disabled={busy} onClick={onReset}>
              <KeyRound aria-hidden="true" size={14} strokeWidth={1.5} />
              Reset password
            </Button>
          )}
          <Button variant="ghost" size="sm" disabled={busy} onClick={onDelete}>
            Delete
          </Button>
          {base && (
            <>
              {copied && (
                <span role="status" className="text-xs text-chalk-dim">
                  {copied}
                </span>
              )}
              <Menu
                trigger={
                  <Button variant="outline" size="icon" aria-label={`More actions for ${login.role_name} on ${login.database_name}`}>
                    <MoreHorizontal aria-hidden="true" size={16} strokeWidth={1.5} />
                  </Button>
                }
              >
                <MenuItem onClick={() => void copy(withDefaultRole(base, role))}>Copy connection string</MenuItem>
                {groups.length > 0 && (
                  <>
                    <MenuNote>Default role for the string</MenuNote>
                    {["", ...groups].map((g) => (
                      <MenuItem key={g} onClick={() => setRole(g)}>
                        <span aria-hidden="true" className="inline-flex size-4 items-center justify-center">
                          {role === g && <Check size={14} strokeWidth={1.5} />}
                        </span>
                        <span className={g === "" ? undefined : "font-mono text-[13px]"}>{g === "" ? "None" : escapeName(g)}</span>
                      </MenuItem>
                    ))}
                  </>
                )}
              </Menu>
            </>
          )}
        </div>
      </td>
    </tr>
  );
}

function DeleteLogin({ login, memberId, who, onClose, onDeleted }: { login: PersonalLoginView; memberId: string; who: string; onClose: () => void; onDeleted: () => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function run() {
    setBusy(true);
    setError(null);
    try {
      await deletePersonalLogin(login.database_id, memberId);
      onDeleted();
    } catch (err) {
      setError(errorText(err));
      setBusy(false);
    }
  }
  return (
    <ConfirmDialog
      open
      onOpenChange={(next) => !next && onClose()}
      title={`Delete the login ${login.role_name}?`}
      body={
        <p>
          The login {escapeName(login.role_name)} on {login.database_name} stops working at once and its open sessions end. {who} can create a new login afterwards.
        </p>
      }
      confirm="Delete login"
      tone="destructive"
      busy={busy}
      error={error}
      onConfirm={() => void run()}
    />
  );
}

interface NewLoginDialogProps {
  member: Member;
  me: MeResponse;
  targets: Target[];
  onClose: () => void;
  /** The login exists and its password is on screen (the reveal dialog holds it). */
  onCreated: () => void;
}

function NewLoginDialog({ member, me, targets, onClose, onCreated }: NewLoginDialogProps) {
  const [projectId, setProjectId] = useState(targets[0]?.project.id ?? "");
  const target = targets.find((t) => t.project.id === projectId) ?? targets[0];
  const [databaseId, setDatabaseId] = useState(target?.databases[0]?.database_id ?? "");
  const database = target?.databases.find((d) => d.database_id === databaseId) ?? target?.databases[0];
  const [name, setName] = useState(() => defaultLoginName(member.email));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [shown, setShown] = useState(false);
  const ids = { project: useId(), database: useId(), name: useId() };
  if (!target || !database) return null;

  function submit(e: FormEvent) {
    e.preventDefault();
    if (busy || !database) return;
    if (!LOGIN_NAME.test(name)) {
      setError(LOGIN_NAME_RULE);
      return;
    }
    setBusy(true);
    setError(null);
    setCreating(true);
  }

  function failed(err: unknown) {
    if (!database) return;
    if (err instanceof ApiError && err.code === "personal_login_exists") {
      setError(`You already have a login on ${database.name}. Delete it first, or reset its password.`);
    } else if (err instanceof ApiError && err.code === "role_exists") {
      setError(`A role named ${name} already exists on ${database.name}. Choose another name.`);
    } else setError(errorText(err));
    setBusy(false);
    setCreating(false);
  }

  return (
    <>
    {creating && (
      <RevealDialog
        request={() => createPersonalLogin(database.database_id, { name })}
        me={me}
        onLoaded={() => {
          setShown(true);
          onCreated();
        }}
        onFailed={failed}
        onClose={onClose}
      />
    )}
    <Modal open={!shown} onOpenChange={(next) => !next && !busy && onClose()} title="New login" description="One login per database, with the same name on every branch.">
      <form noValidate onSubmit={(e) => void submit(e)} className="space-y-4">
        <div className="space-y-1">
          <label htmlFor={ids.project} className="block text-sm font-medium">
            Project
          </label>
          <Select
            id={ids.project}
            value={target.project.id}
            onChange={(e) => {
              setProjectId(e.target.value);
              setDatabaseId(targets.find((t) => t.project.id === e.target.value)?.databases[0]?.database_id ?? "");
            }}
            >
            {targets.map((t) => (
              <option key={t.project.id} value={t.project.id}>
                {t.project.name}
              </option>
            ))}
          </Select>
        </div>
        <div className="space-y-1">
          <label htmlFor={ids.database} className="block text-sm font-medium">
            Database
          </label>
          <Select id={ids.database} value={database.database_id} onChange={(e) => setDatabaseId(e.target.value)} >
            {target.databases.map((d) => (
              <option key={d.database_id} value={d.database_id}>
                {d.name}
              </option>
            ))}
          </Select>
        </div>
        <div className="space-y-1">
          <label htmlFor={ids.name} className="block text-sm font-medium">
            Login name
          </label>
          <input
            id={ids.name}
            value={name}
            onChange={(e) => setName(e.target.value)}
            autoCapitalize="none"
            autoComplete="off"
            spellCheck={false}
            className={cn(field, "block w-full font-mono text-[13px] font-normal")}
          />
        </div>
        {error && (
          <p role="alert" className="text-sm text-coral">
            {error}
          </p>
        )}
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" disabled={busy} onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" disabled={busy}>
            {busy ? "Creating…" : "Create login"}
          </Button>
        </div>
      </form>
    </Modal>
    </>
  );
}
