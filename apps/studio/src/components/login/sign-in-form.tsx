import { useEffect, useState, type FormEvent, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { LoginApiError, type LoginApi, type LoginNext } from "@/lib/login-api";

export const EXPIRED = "This sign-in has expired, start again";

export interface FormProps {
  api: LoginApi;
  authRequest?: string;
  initialCode?: string;
  go: (url: string) => void;
  toVerify: () => void;
  toRegister?: () => void;
  /** Sign-up found the email taken: go to sign-in with ui-auth's message. */
  toLogin?: (notice: string) => void;
  /** Shown above the sign-in form; also keeps it from forwarding to sign-up. */
  notice?: string;
}

export function messageOf(err: unknown): string {
  return err instanceof LoginApiError ? err.message : "Sign-in is unavailable, try again";
}

export function follow(next: LoginNext, go: (url: string) => void, toVerify: () => void) {
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

export function SignInForm({ api, authRequest = "", go, toVerify, toRegister, notice }: FormProps) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(authRequest ? null : EXPIRED);

  useEffect(() => {
    if (!authRequest) return;
    api.authRequest(authRequest).then(
      ({ signUp }) => { if (signUp && !notice) toRegister?.(); },
      (err) => setError(messageOf(err)),
    );
  }, [api, authRequest, toRegister, notice]);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await api.signIn({ authRequest, email, password }), go, toVerify);
    } catch (err) {
      setError(messageOf(err));
      setBusy(false);
    }
  }

  return (
    <LoginLayout title="Sign in to Cheladb">
      {notice && <p role="status" className="text-sm">{notice}</p>}
      <form onSubmit={submit} className="space-y-4">
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        <Field label="Password" type="password" value={password} onChange={setPassword} autoComplete="current-password" />
        {error && <Problem message={error} restart={error === EXPIRED} />}
        <Button type="submit" disabled={busy || !authRequest} className="w-full">Sign in</Button>
      </form>
      <p className="text-sm text-muted-foreground">
        <a href="/password-reset" className="underline">Forgot your password?</a>
      </p>
    </LoginLayout>
  );
}
