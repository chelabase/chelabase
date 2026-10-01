import { ApiError } from "@cheladb/management-client";
import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { useSignUp } from "@/lib/login-client/generated/endpoints";
import {
  EXPIRED_PROBLEM, Field, LoginLayout, Problem, SECRET, follow, problemOf, type FormProps, type LoginProblem,
} from "./sign-in-form";

export function SignUpForm({ authRequest = "", go, toVerify, toLogin }: FormProps) {
  const [givenName, setGivenName] = useState("");
  const [familyName, setFamilyName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<LoginProblem | null>(authRequest ? null : EXPIRED_PROBLEM);
  const signUp = useSignUp(SECRET);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await signUp.mutateAsync({ data: { authRequest, email, password, givenName, familyName } }), go, toVerify);
    } catch (err) {
      if (err instanceof ApiError && err.code === "email_taken" && toLogin) {
        toLogin(err.message);
        return;
      }
      setError(problemOf(err));
      setBusy(false);
    } finally {
      signUp.reset();
    }
  }

  return (
    <LoginLayout title="Create your Cheladb account">
      <form onSubmit={submit} className="space-y-4">
        <Field label="First name" value={givenName} onChange={setGivenName} autoComplete="given-name" />
        <Field label="Last name" value={familyName} onChange={setFamilyName} autoComplete="family-name" />
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        <Field label="Password" type="password" value={password} onChange={setPassword} autoComplete="new-password" />
        {error && <Problem message={error.message} restart={error.restart} />}
        <Button type="submit" disabled={busy || !authRequest} className="w-full">Create account</Button>
      </form>
    </LoginLayout>
  );
}
