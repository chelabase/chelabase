import { useEffect, useRef, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useGetMe, type MeResponse as Me } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { memberGate } from "@/lib/capabilities";
import { errorText } from "@/lib/errors";
import { CopyButton } from "./copy-button";

/** How often a pending caller's `me` is checked. */
export const APPROVAL_POLL_MS = 30_000;

const isPending = (me: Me | undefined) => memberGate(me) === "pending";

/**
 * Renders the app, unless `/v1/me` says the caller's sign-up still waits for approval (R39, R40):
 * every other call answers 403 `pending_approval`, so the whole app gives way to one screen.
 */
export function PendingGate({ onSignOut, children }: { onSignOut: () => void; children: ReactNode }) {
  const me = useGetMe({ query: { refetchInterval: (q) => (isPending(q.state.data) ? APPROVAL_POLL_MS : false) } });
  const queryClient = useQueryClient();
  // Nothing of the app until `me` says who this is: a pending caller would only meet 403s.
  if (me.data === undefined) {
    if (me.error?.signInRestarted) return <Centered><p className="text-sm text-chalk-dim">Your session expired. Signing you in again…</p></Centered>;
    if (me.error) {
      return (
        <Centered>
          <p role="alert" className="text-sm text-coral">
            Couldn't load your account: {errorText(me.error)}
          </p>
          <Button variant="outline" disabled={me.isFetching} onClick={() => void me.refetch()}>
            {me.isFetching ? "Retrying…" : "Retry"}
          </Button>
        </Centered>
      );
    }
    return <Centered><p className="text-sm text-chalk-dim">Loading your account…</p></Centered>;
  }
  const signOut = () => {
    // Nothing of this session may outlive it in the cache.
    queryClient.clear();
    onSignOut();
  };
  const gate = memberGate(me.data);
  if (gate === "removed") return <Removed me={me.data} onSignOut={signOut} />;
  if (gate === "invite-expired") return <InviteExpired me={me.data} onSignOut={signOut} />;
  if (gate !== "pending") return <>{children}</>;
  return (
    <WaitingForApproval
      me={me.data!}
      checking={me.isFetching}
      refreshError={me.error ? errorText(me.error) : null}
      onCheck={() => void me.refetch()}
      onSignOut={signOut}
    />
  );
}

/** A removed member's only screen (M6): their access is gone, and so is every call; they can only sign out. */
function Removed({ me, onSignOut }: { me: Me; onSignOut: () => void }) {
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => heading.current?.focus(), []);
  return (
    <Centered>
      <h1 ref={heading} tabIndex={-1} className="text-2xl font-semibold tracking-tight outline-none">
        You were removed from this tenant
      </h1>
      <p className="text-sm text-chalk">
        {me.email ? `${me.email} no longer has access to ` : "You no longer have access to "}
        {me.tenant ? me.tenant.display_name || me.tenant.slug : "this workspace"}. If that's a mistake, ask a tenant admin to
        invite you again.
      </p>
      <Button variant="outline" onClick={onSignOut}>
        Sign out
      </Button>
    </Centered>
  );
}

/**
 * R49: signed in while still `invited` means the invite expired before it was accepted (an open
 * one would have activated them). Every call refuses them, so this is the only screen: whom to
 * ask for a new invite (`me` names the tenant admins), and a way out.
 */
function InviteExpired({ me, onSignOut }: { me: Me; onSignOut: () => void }) {
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => heading.current?.focus(), []);
  const tenant = me.tenant ? me.tenant.display_name || me.tenant.slug : "this workspace";
  const admins = me.tenant_admins ?? [];
  return (
    <Centered>
      <h1 ref={heading} tabIndex={-1} className="text-2xl font-semibold tracking-tight outline-none">
        Your invite expired
      </h1>
      <p className="text-sm text-chalk">Your invite to {tenant} expired. Ask an admin to send a new one.</p>
      {admins.length > 0 && (
        <div className="space-y-2">
          <h2 id="invite-expired-admins" className="text-sm font-semibold">
            Tenant admins
          </h2>
          <ul aria-labelledby="invite-expired-admins" className="border-y border-line">
            {admins.map((a, i) => (
              <li key={`${a.email ?? ""}-${i}`} className="flex min-h-9 flex-wrap items-center gap-x-3 border-b border-line py-1.5 text-sm last:border-b-0">
                <span className="text-chalk">{a.name || "Unnamed admin"}</span>
                {a.email ? (
                  <>
                    <span className="text-chalk-dim">{a.email}</span>
                    <CopyButton value={a.email} label={`Copy ${a.email}`} />
                  </>
                ) : (
                  <span className="text-chalk-dim">Email unknown</span>
                )}
              </li>
            ))}
          </ul>
        </div>
      )}
      <Button variant="outline" onClick={onSignOut}>
        Sign out
      </Button>
    </Centered>
  );
}

function Centered({ children }: { children: ReactNode }) {
  return (
    <main className="flex min-h-screen items-center justify-center bg-background p-8 text-foreground">
      <div className="w-full max-w-sm space-y-4">{children}</div>
    </main>
  );
}

export interface WaitingForApprovalProps {
  me: Me;
  checking: boolean;
  refreshError: string | null;
  onCheck: () => void;
  onSignOut: () => void;
}

/** The pending caller's only screen: who they're signed in as, who decides, and a way out. */
export function WaitingForApproval({ me, checking, refreshError, onCheck, onSignOut }: WaitingForApprovalProps) {
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => heading.current?.focus(), []);
  const tenant = me.tenant ? me.tenant.display_name || me.tenant.slug : "this workspace";
  return (
    <main className="flex min-h-screen items-center justify-center bg-background p-8 text-foreground">
      <div className="w-full max-w-sm space-y-6">
        <h1 ref={heading} tabIndex={-1} className="text-2xl font-semibold tracking-tight outline-none">
          Waiting for approval
        </h1>
        <p className="text-sm text-chalk">
          {me.email ? `You're signed in as ${me.email}. ` : ""}A tenant admin of {tenant} needs to approve your account
          before you can use Chelabase Studio.
        </p>
        <p className="text-sm text-chalk-dim">This page checks every 30 seconds and opens Studio as soon as you're approved.</p>
        {refreshError && <p className="text-sm text-chalk-dim">Couldn't check just now ({refreshError}); trying again.</p>}
        <div className="flex gap-2">
          <Button variant="outline" disabled={checking} onClick={onCheck}>
            {checking ? "Checking…" : "Check now"}
          </Button>
          <Button variant="ghost" onClick={onSignOut}>
            Sign out
          </Button>
        </div>
      </div>
    </main>
  );
}
