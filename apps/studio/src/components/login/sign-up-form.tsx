import { ApiError } from "@chelabase/management-client";
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { useGetInstance, useSignUp } from "@/lib/login-client/generated/endpoints";
import {
  EXPIRED_PROBLEM, Field, LoginLayout, Problem, SECRET, follow, problemOf, type FormProps, type LoginProblem,
} from "./sign-in-form";

export function SignUpForm({ authRequest = "", go, toVerify, toLogin }: FormProps) {
  const [givenName, setGivenName] = useState("");
  const [familyName, setFamilyName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [setupCode, setSetupCode] = useState("");
  // Auth answered `setup_code_invalid`: ask for the code even if the instance check failed.
  const [codeAsked, setCodeAsked] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<LoginProblem | null>(authRequest ? null : EXPIRED_PROBLEM);
  const signUp = useSignUp(SECRET);
  const instance = useGetInstance({ query: { retry: false, staleTime: Infinity } });
  const showCode = instance.data?.setupRequired === true || codeAsked;

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const code = showCode ? setupCode.trim() : "";
      const data = { authRequest, email, password, givenName, familyName, ...(code ? { setupCode: code } : {}) };
      follow(await signUp.mutateAsync({ data }), go, toVerify);
    } catch (err) {
      if (err instanceof ApiError && err.code === "email_taken" && toLogin) {
        toLogin(err.message);
        return;
      }
      if (err instanceof ApiError && err.code === "setup_code_invalid") setCodeAsked(true);
      setError(problemOf(err));
      setBusy(false);
    } finally {
      signUp.reset();
    }
  }

  return (
    <LoginLayout title="Create your Chelabase account">
      <form onSubmit={submit} className="space-y-4">
        <Field label="First name" value={givenName} onChange={setGivenName} autoComplete="given-name" />
        <Field label="Last name" value={familyName} onChange={setFamilyName} autoComplete="family-name" />
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        <Field label="Password" type="password" value={password} onChange={setPassword} autoComplete="new-password" />
        {showCode && <SetupCodeField value={setupCode} onChange={setSetupCode} />}
        {error && <Problem message={error.message} restart={error.restart} />}
        <Button type="submit" disabled={busy || !authRequest} className="w-full">Create account</Button>
      </form>
    </LoginLayout>
  );
}

/** The first account of an unclaimed instance needs the setup code `chelabase-init secrets` printed. */
function SetupCodeField({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  return (
    <div className="space-y-1">
      <label htmlFor="setup-code" className="text-sm font-medium">Setup code</label>
      <input id="setup-code" type="text" value={value} autoComplete="off" spellCheck={false} required
        aria-describedby="setup-code-help" onChange={(e) => onChange(e.target.value)}
        className="w-full border border-input bg-background px-3 py-2 font-mono text-sm" />
      <p id="setup-code-help" className="text-xs text-muted-foreground">
        This instance has no administrator yet; the first account needs the setup code printed at install.
        Find it with <code>docker compose logs init-secrets</code> or{" "}
        <code>docker compose run --rm init-secrets chelabase-init setup-code</code>.
      </p>
    </div>
  );
}
