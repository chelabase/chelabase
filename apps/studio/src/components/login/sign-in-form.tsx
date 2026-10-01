import { ApiError } from "@cheladb/management-client";
import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { useGetAuthRequest, useSignIn } from "@/lib/login-client/generated/endpoints";
import type { NextStep, SignedIn } from "@/lib/login-client/generated/model";

export const EXPIRED = "This sign-in has expired, start again";
const UNAVAILABLE = "Sign-in is unavailable, try again";

/**
 * Options for mutations whose variables hold a password: dropped from the
 * mutation cache as soon as nothing observes them (pair with `reset()`).
 */
export const SECRET = { mutation: { gcTime: 0 } };

export interface FormProps {
  authRequest?: string;
  initialCode?: string;
  go: (url: string) => void;
  toVerify: () => void;
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

/** What a failed login call shows: auth's message, plus a restart link when the sign-in is over. */
export interface LoginProblem {
  message: string;
  restart: boolean;
}

export const EXPIRED_PROBLEM: LoginProblem = { message: EXPIRED, restart: true };

export function problemOf(err: unknown): LoginProblem {
  return { message: messageOf(err), restart: err instanceof ApiError && err.code === "sign_in_expired" };
}

export function follow(next: NextStep | SignedIn, go: (url: string) => void, toVerify: () => void) {
  if (next.next === "done") go(next.redirect);
  else toVerify();
}

export function LoginLayout({ title, children }: { title: string; children: ReactNode }) {
  return (
    <main className="flex min-h-screen items-center justify-center bg-background p-8 text-foreground">
      <div className="w-full max-w-sm space-y-6">
        <h1 className="text-2xl font-semibold tracking-tight">{title}</h1>
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
        className="w-full rounded-md border border-input bg-background px-3 py-2 text-sm" />
    </div>
  );
}

export function Problem({ message, restart }: { message: string; restart?: boolean }) {
  return (
    <div role="alert" className="text-sm text-destructive">
      {message}
      {restart && (<> <a href="/" className="underline">Start again</a></>)}
    </div>
  );
}

export function SignInForm({ authRequest = "", go, toVerify, toRegister, notice }: FormProps) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<LoginProblem | null>(authRequest ? null : EXPIRED_PROBLEM);
  const { mutate: lookUp } = useGetAuthRequest();
  const signIn = useSignIn(SECRET);

  useEffect(() => {
    if (!authRequest) return;
    lookUp({ data: { authRequest } }, {
      onSuccess: ({ signUp }) => { if (signUp && !notice) toRegister?.(); },
      onError: (err) => setError(problemOf(err)),
    });
  }, [lookUp, authRequest, toRegister, notice]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await signIn.mutateAsync({ data: { authRequest, email, password } }), go, toVerify);
    } catch (err) {
      setError(problemOf(err));
      setBusy(false);
    } finally {
      signIn.reset();
    }
  }

  return (
    <LoginLayout title="Sign in to Cheladb">
      {notice && <p role="status" className="text-sm">{notice}</p>}
      <form onSubmit={submit} className="space-y-4">
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        <Field label="Password" type="password" value={password} onChange={setPassword} autoComplete="current-password" />
        {error && <Problem message={error.message} restart={error.restart} />}
        <Button type="submit" disabled={busy || !authRequest} className="w-full">Sign in</Button>
      </form>
      <p className="text-sm text-muted-foreground">
        <a href="/password-reset" className="underline">Forgot your password?</a>
      </p>
    </LoginLayout>
  );
}
