import { ApiError } from "@chelabase/management-client";
import { useState, type FormEvent, type ReactNode, type Ref } from "react";
import { Button } from "@/components/ui/button";
import { useGetInstance, useSignIn } from "@/lib/login-client/generated/endpoints";
import type { SignInAnswer } from "@/lib/login-client/generated/model";

const UNAVAILABLE = "Sign-in is unavailable, try again";

/**
 * Options for mutations whose variables hold a password: dropped from the
 * mutation cache as soon as nothing observes them (pair with `reset()`).
 */
export const SECRET = { mutation: { gcTime: 0 } };

export interface FormProps {
  initialCode?: string;
  /** Signed in: the cookie session is set; open the app. */
  toApp: () => void;
  /** The email needs its code first. */
  toVerify: () => void;
  /** An admin hasn't approved this person yet. */
  toPending: () => void;
  toRegister?: () => void;
  /** Sign-up found the email taken: go to sign-in with auth's message. */
  toLogin?: (notice: string) => void;
  /** Shown above the sign-in form; also keeps it from forwarding to sign-up. */
  notice?: string;
}

/** Codes for "auth or the way to it is down": the gateway's or the client's wording, not auth's. */
const UNAVAILABLE_CODES: ReadonlySet<string> = new Set(["network", "unavailable", "timeout", "internal"]);

export function messageOf(err: unknown): string {
  return err instanceof ApiError && !UNAVAILABLE_CODES.has(err.code) ? err.message : UNAVAILABLE;
}

/** What a failed login call shows: auth's message, plus a restart link when the code step's cookie has expired. */
export interface LoginProblem {
  message: string;
  restart: boolean;
}

export function problemOf(err: unknown): LoginProblem {
  return { message: messageOf(err), restart: err instanceof ApiError && err.code === "sign_in_expired" };
}

/** Where a sign-in's answer (sign-in, sign-up, verify-email, accept-invite) sends the person. */
export function follow(answer: SignInAnswer, to: Pick<FormProps, "toApp" | "toVerify" | "toPending">) {
  switch (answer.next) {
    case "studio":
      return to.toApp();
    case "verify":
      return to.toVerify();
    case "pending":
      return to.toPending();
  }
}

export function LoginLayout({ title, titleRef, children }: { title: string; titleRef?: Ref<HTMLHeadingElement>; children: ReactNode }) {
  return (
    <main className="flex min-h-screen items-center justify-center bg-background p-8 text-foreground">
      <div className="w-full max-w-sm space-y-6">
        <h1 ref={titleRef} tabIndex={titleRef ? -1 : undefined} className="text-2xl font-semibold tracking-tight outline-none">
          {title}
        </h1>
        {children}
      </div>
    </main>
  );
}

export function Field({ label, type = "text", value, onChange, autoComplete }: {
  label: string; type?: string; value: string; onChange: (v: string) => void; autoComplete?: string;
}) {
  const id = label.toLowerCase().replace(/\s+/g, "-");
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="text-sm font-medium">{label}</label>
      <input id={id} type={type} value={value} autoComplete={autoComplete} required
        onChange={(e) => onChange(e.target.value)}
        className="w-full border border-input bg-background px-3 py-2 text-sm" />
    </div>
  );
}

export function Problem({ message, restart }: { message: string; restart?: boolean }) {
  return (
    <div role="alert" className="text-sm text-destructive">
      {message}
      {restart && (<> <a href="/login" className="underline">Start again</a></>)}
    </div>
  );
}

/** The tenant ids a `tenant_required` answer lists; null for any other error. */
function tenantsOf(err: unknown): string[] | null {
  if (!(err instanceof ApiError) || err.code !== "tenant_required") return null;
  const list = err.extra.tenants;
  return Array.isArray(list) && list.length > 0 && list.every((t) => typeof t === "string") ? (list as string[]) : null;
}

export function SignInForm({ toApp, toVerify, toPending, toRegister, notice }: FormProps) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<LoginProblem | null>(null);
  // Several tenants have this person: the ids to choose from, until one is picked.
  const [tenants, setTenants] = useState<string[] | null>(null);
  const signIn = useSignIn(SECRET);
  // "Create account" only while the instance takes sign-ups; unknown (still loading, or the
  // check failed) offers the invite line alone.
  const instance = useGetInstance({ query: { retry: false, staleTime: Infinity } });
  const offerSignUp = toRegister !== undefined && instance.data?.signUpAllowed === true;

  async function attempt(tenant?: string) {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await signIn.mutateAsync({ data: { email, password }, ...(tenant ? { params: { tenant } } : {}) }), { toApp, toVerify, toPending });
    } catch (err) {
      const choices = tenant ? null : tenantsOf(err);
      if (choices) setTenants(choices);
      else setError(problemOf(err));
      setBusy(false);
    } finally {
      signIn.reset();
    }
  }

  function submit(event: FormEvent) {
    event.preventDefault();
    void attempt();
  }

  if (tenants) {
    return (
      <LoginLayout title="Choose a workspace">
        <p className="text-sm text-muted-foreground">Your account is in more than one. Pick the one to sign in to.</p>
        <ul className="space-y-2">
          {tenants.map((id) => (
            <li key={id}>
              <Button type="button" variant="outline" disabled={busy} onClick={() => void attempt(id)} className="w-full justify-start font-mono">
                {id}
              </Button>
            </li>
          ))}
        </ul>
        {error && <Problem message={error.message} restart={error.restart} />}
        <Button type="button" variant="ghost" onClick={() => { setTenants(null); setError(null); }}>Back</Button>
      </LoginLayout>
    );
  }

  return (
    <LoginLayout title="Sign in to Chelabase">
      {notice && <p role="status" className="text-sm">{notice}</p>}
      <form onSubmit={submit} className="space-y-4">
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        <Field label="Password" type="password" value={password} onChange={setPassword} autoComplete="current-password" />
        {error && <Problem message={error.message} restart={error.restart} />}
        <Button type="submit" disabled={busy} className="w-full">Sign in</Button>
      </form>
      <p className="text-sm text-muted-foreground">
        <a href="/password-reset" className="underline">Forgot your password?</a>
      </p>
      <p className="text-sm text-muted-foreground">
        {offerSignUp ? (
          <>No account? <button type="button" onClick={toRegister} className="underline">Create account</button> or ask your admin for an invite.</>
        ) : (
          "No account? Ask your admin for an invite."
        )}
      </p>
    </LoginLayout>
  );
}
