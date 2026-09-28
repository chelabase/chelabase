import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { LoginApiError } from "@/lib/login-api";
import { EXPIRED, Field, LoginLayout, Problem, follow, messageOf, type FormProps } from "./sign-in-form";

export function SignUpForm({ api, authRequest = "", go, toVerify, toLogin }: FormProps) {
  const [givenName, setGivenName] = useState("");
  const [familyName, setFamilyName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(authRequest ? null : EXPIRED);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await api.signUp({ authRequest, email, password, givenName, familyName }), go, toVerify);
    } catch (err) {
      if (err instanceof LoginApiError && err.status === 409 && toLogin) {
        toLogin(err.message);
        return;
      }
      setError(messageOf(err));
      setBusy(false);
    }
  }

  return (
    <LoginLayout title="Create your Cheladb account">
      <form onSubmit={submit} className="space-y-4">
        <Field label="First name" value={givenName} onChange={setGivenName} autoComplete="given-name" />
        <Field label="Last name" value={familyName} onChange={setFamilyName} autoComplete="family-name" />
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        <Field label="Password" type="password" value={password} onChange={setPassword} autoComplete="new-password" />
        {error && <Problem message={error} restart={error === EXPIRED} />}
        <Button type="submit" disabled={busy || !authRequest} className="w-full">Create account</Button>
      </form>
    </LoginLayout>
  );
}
