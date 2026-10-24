import { useEffect, useId, useRef, useState } from "react";
import type { MeResponse, PersonalLoginSecret } from "@chelabase/management-client";
import { CopyIconButton } from "@/components/copy-button";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { escapeName } from "../hub";
import { DefaultRoleSelect, useGroupRoles } from "./default-role";
import { dateText, withDefaultRole } from "./logins";

const box = "min-w-0 flex-1 border border-input bg-ink px-2 font-mono text-[13px] text-chalk outline-none focus-visible:outline-2 focus-visible:-outline-offset-1 focus-visible:outline-sand";

export interface RevealDialogProps {
  /**
   * Creates or resets the login and answers with its password. The dialog runs it itself and keeps
   * the answer in its own state only (MH-38c, EOS-H3): no ancestor ever holds the password. Nothing
   * shows until it answers.
   */
  request: () => Promise<PersonalLoginSecret>;
  me: MeResponse | undefined;
  /** The request answered and the dialog is now showing. */
  onLoaded?: () => void;
  /** The request failed; the dialog never opens. */
  onFailed: (err: unknown) => void;
  onClose: () => void;
}

/** The password and connection string, shown once. No click-outside dismissal; Done, Escape and the close all clear it. */
export function RevealDialog({ request, me, onLoaded, onFailed, onClose }: RevealDialogProps) {
  const [secret, setSecret] = useState<PersonalLoginSecret | null>(null);
  const started = useRef(false);
  useEffect(() => {
    // Once only, even if effects run twice: the request resets a password.
    if (started.current) return;
    started.current = true;
    request().then(
      (s) => {
        setSecret(s);
        onLoaded?.();
      },
      onFailed,
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  if (!secret) return null;
  return <RevealBody secret={secret} me={me} onClose={() => (setSecret(null), onClose())} />;
}

function RevealBody({ secret, me, onClose }: { secret: PersonalLoginSecret; me: MeResponse | undefined; onClose: () => void }) {
  const { login } = secret;
  const passwordId = useId();
  const stringId = useId();
  const [role, setRole] = useState("");
  const project = me?.tenant?.projects.find((p) => p.id === login.project_id);
  const database = project?.databases.find((d) => d.database_id === login.database_id);
  const groups = useGroupRoles(me, login.project_id, database);
  const connection = withDefaultRole(secret.connection_string, role);

  return (
    <Modal
      open
      onOpenChange={(next) => !next && onClose()}
      size="medium"
      disablePointerDismissal
      title={`Your login ${login.role_name} on ${login.database_name}`}
      description={`${project?.name ?? "This project"} · expires ${dateText(login.valid_until)} (90 days). Reset the password before then to renew it.`}
    >
      <div className="space-y-4">
        <div className="space-y-1">
          <label htmlFor={passwordId} className="block text-sm font-medium">
            Password
          </label>
          <div className="flex items-stretch">
            <input id={passwordId} readOnly value={secret.password} onFocus={(e) => e.currentTarget.select()} className={`${box} h-9`} />
            <CopyIconButton value={secret.password} label="Copy password" className="-ml-px" />
          </div>
        </div>
        <div className="space-y-1">
          <label htmlFor={stringId} className="block text-sm font-medium">
            Connection string · main branch
          </label>
          <div className="flex items-stretch">
            {/* A textarea, so a long string wraps and is all visible (no scrolling a single line). */}
            <textarea id={stringId} readOnly rows={4} value={connection} onFocus={(e) => e.currentTarget.select()} className={`${box} resize-none py-1.5 break-all`} />
            <CopyIconButton value={connection} label="Copy connection string" className="-ml-px" />
          </div>
          <p className="text-xs text-chalk-dim">
            The same login works on every branch where you have Developer access or more; on another branch, use that branch's host.
          </p>
        </div>
        <DefaultRoleSelect roles={groups} value={role} onChange={setRole} label="Default role" />
        <p className="border border-line bg-ink p-3 text-sm text-chalk">
          Your roles here:{" "}
          {groups.length === 0 ? (
            "none yet"
          ) : (
            groups.map((g, i) => (
              <span key={g}>
                {i > 0 && ", "}
                <span className="font-mono text-[13px]">{escapeName(g)}</span>
              </span>
            ))
          )}
          .
          {groups.length > 0 && (
            <>
              {" "}
              In psql, run <span className="font-mono text-[13px]">SET ROLE {escapeName(groups[0])};</span> to act as one.
            </>
          )}
        </p>
        <p className="text-sm font-semibold text-chalk">You won't see this password again. Copy it now.</p>
        <div className="flex justify-end">
          <Button onClick={onClose}>Done</Button>
        </div>
      </div>
    </Modal>
  );
}
