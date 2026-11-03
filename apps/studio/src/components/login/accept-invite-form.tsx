import { useEffect, useLayoutEffect, useRef, useState, type FormEvent } from "react";
import { ApiError } from "@chelabase/management-client";
import { Button } from "@/components/ui/button";
import { describeInvite, useAcceptInvite } from "@/lib/login-client/generated/endpoints";
import type { InviteDescription } from "@/lib/login-client/generated/model";
import { LoginLayout, Problem, SECRET, messageOf } from "./sign-in-form";

export interface AcceptInviteFormProps {
  /** The user id, the invite code and the Chelabase invite token, from the link. */
  user: string;
  code: string;
  t: string;
  /** The link's `reset=1`: the code is a password-reset code (a removed member invited again). */
  reset?: boolean;
  /** Leaves for `url` once the account is set up and the cookie session is set. */
  go: (url: string) => void;
}

type Ending = "closed" | "expired" | "revoked" | "invalid" | "ticket-spent" | "no-ticket";

const ENDINGS: Record<Ending, { title: string; body: string }> = {
  // Before the password: describe-invite doesn't say which (the token is all it checks).
  closed: {
    title: "This invite link no longer works",
    body: "It has expired, been withdrawn or already been used. Ask the person who invited you to send a new invite.",
  },
  expired: {
    title: "This invite has expired",
    body: "Invites work for 72 hours. Ask the person who invited you to send it again; the new email has a fresh link.",
  },
  revoked: { title: "This invite was withdrawn", body: "If you still need access, ask a tenant admin to invite you again." },
  invalid: {
    title: "This invite link doesn't work",
    body: "It may be an older link: only the newest invite works. Ask the person who invited you to send it again.",
  },
  "no-ticket": {
    title: "Setting up your account didn't finish",
    body: "Your invite code was used, so it can't be tried again from here. Open the link from your invite email again, or ask the person who invited you for a new one.",
  },
  "ticket-spent": {
    title: "This invite link doesn't work",
    body: "Setting up your account didn't finish in time. Try signing in with your email and the password you chose; if that fails, ask for a new invite.",
  },
};

const ticketOf = (err: unknown): string | null =>
  err instanceof ApiError && typeof err.extra.accept_ticket === "string" ? err.extra.accept_ticket : null;

/**
 * `/accept-invite`: set a password and land signed in. After the code is spent (a weak password,
 * or a later step failing), the answer's one-use `accept_ticket` replaces the code (R13). The
 * password and ticket live only in this component's state.
 */
export function AcceptInviteForm({
  user,
  code: linkCode,
  t: linkToken,
  reset: linkReset = false,
  go,
}: AcceptInviteFormProps) {
  // M8: the link's code and token are read once into state, and the address bar keeps only
  // `?user=` at once (on mount, not at the first submit), so they stay out of the history, a
  // bookmark or a shared screen. The router then re-renders with the shorter address: the page
  // keeps what it read.
  const [{ code, t, reset }] = useState(() => ({ code: linkCode, t: linkToken, reset: linkReset }));
  useEffect(() => {
    const here = new URL(window.location.href);
    const only = new URLSearchParams(user ? { user } : {});
    if (here.search !== (only.size > 0 ? `?${only}` : "")) {
      window.history.replaceState(window.history.state, "", `${here.pathname}${only.size > 0 ? `?${only}` : ""}${here.hash}`);
    }
  }, [user]);
  const complete = user !== "" && code !== "";
  // Who invited them, when the token checks out; never blocks the form, never retried. A 404
  // (expired, withdrawn or used) ends the page before any password is typed. A plain call into
  // component state: the token never enters a query key, the cache or devtools.
  const [inviter, setInviter] = useState<InviteDescription | null>(null);
  const [looked, setLooked] = useState(t === "");
  const [ending, setEnding] = useState<Ending | null>(null);
  useEffect(() => {
    if (!complete || t === "") return;
    let live = true;
    describeInvite({ user, t }).then(
      (d) => {
        if (!live) return;
        setInviter(d);
        setLooked(true);
      },
      (err: unknown) => {
        if (!live) return;
        if (err instanceof ApiError && err.code === "not_found") setEnding("closed");
        setLooked(true); // Any other failure: the generic page.
      },
    );
    return () => {
      live = false;
    };
  }, [complete, user, t]);
  const accept = useAcceptInvite(SECRET);
  const [password, setPassword] = useState("");
  const [ticket, setTicket] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [problem, setProblem] = useState<{ message: string; retry: boolean } | null>(null);
  const field = useRef<HTMLInputElement>(null);
  const title = useRef<HTMLHeadingElement>(null);

  // A layout effect: the heading is focused in the commit that inserts it, never a task later.
  useLayoutEffect(() => {
    if (ending || !complete) title.current?.focus();
  }, [ending, complete]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setProblem(null);
    const usedTicket = ticket;
    try {
      const done = await accept.mutateAsync({
        data: {
          ...(usedTicket ? { user, acceptTicket: usedTicket, password } : reset ? { user, code, reset: true, password } : { user, code, password }),
        },
      });
      // Stay busy until the browser leaves.
      go(done.next === "done" ? done.redirect : "/app");
      return;
    } catch (err) {
      const next = ticketOf(err);
      if (next) setTicket(next);
      const code_ = err instanceof ApiError ? err.code : "network";
      // The code is spent once these come back; without a ticket there is nothing to retry with.
      const spentWithoutTicket = (code_ === "weak_password" || code_ === "invite_incomplete") && !(next ?? usedTicket);
      if (spentWithoutTicket) setEnding("no-ticket");
      else if (code_ === "invite_expired") setEnding("expired");
      else if (code_ === "invite_revoked") setEnding("revoked");
      else if (code_ === "invalid_request") setEnding(usedTicket ? "ticket-spent" : "invalid");
      else if (code_ === "weak_password") {
        setProblem({ message: messageOf(err), retry: false });
        setPassword("");
        field.current?.focus();
      } else if (code_ === "invite_incomplete") {
        setProblem({ message: "Your invite code is confirmed, but setting up your account didn't finish. Try again.", retry: true });
      } else if (code_ === "member_suspended") {
        // The invite is accepted and the password set; only the suspension stands in the way.
        setProblem({
          message: "Your invite is accepted, but your access is suspended. Once an admin reinstates you, sign in with the password you just set.",
          retry: false,
        });
      } else if (code_ === "rate_limited") {
        setProblem({ message: "Too many attempts. Wait a minute, then try again.", retry: false });
        field.current?.focus();
      } else {
        setProblem({ message: messageOf(err), retry: false });
        field.current?.focus();
      }
      setBusy(false);
    } finally {
      accept.reset();
    }
  }

  if (!complete || ending) {
    const copy = ending
      ? ENDINGS[ending]
      : {
          title: "This invite link is incomplete",
          body: "Open the link from your invite email again. If it still doesn't work, ask the person who invited you to send a new invite.",
        };
    return (
      <LoginLayout title={copy.title} titleRef={title}>
        <p className="text-sm text-chalk">{copy.body}</p>
        {ending === "ticket-spent" ? (
          <a href="/app" className="text-sm underline underline-offset-4">
            Sign in
          </a>
        ) : (
          <a href="/" className="text-sm underline underline-offset-4">
            Back to Chelabase Studio
          </a>
        )}
      </LoginLayout>
    );
  }

  // The inviter lookup still runs.
  if (!looked) {
    return (
      <LoginLayout title="Join Chelabase" titleRef={title}>
        <p className="text-sm text-chalk-dim">Opening your invite…</p>
      </LoginLayout>
    );
  }


  return (
    <LoginLayout title="Join Chelabase" titleRef={title}>
      {inviter && (
        <p className="text-sm text-chalk">
          {inviter.inviter_name ? (
            <>
              {inviter.inviter_name}
              {inviter.inviter_email && <span className="text-chalk-dim"> ({inviter.inviter_email})</span>}
            </>
          ) : (
            inviter.inviter_email
          )}{" "}
          invited you to {inviter.tenant_name}.
        </p>
      )}
      <p className="text-sm text-chalk-dim">Set a password to accept your invite. You'll use it with your email to sign in.</p>
      <form onSubmit={submit} className="space-y-4">
        <div className="space-y-1">
          <label htmlFor="password" className="text-sm font-medium">
            Password
          </label>
          <input
            ref={field}
            id="password"
            type="password"
            autoComplete="new-password"
            autoFocus
            required
            readOnly={busy}
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            className="w-full border border-input bg-background px-3 py-2 text-sm"
          />
        </div>
        {problem && <Problem message={problem.message} />}
        <Button type="submit" disabled={busy || password === ""} className="w-full">
          {busy ? "Setting your password…" : problem?.retry ? "Try again" : "Set password and sign in"}
        </Button>
      </form>
    </LoginLayout>
  );
}
