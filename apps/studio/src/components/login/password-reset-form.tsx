import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import type { LoginApi } from "@/lib/login-api";
import { Field, LoginLayout, Problem, messageOf } from "./sign-in-form";

export function PasswordResetForm({ api, userId = "", code = "" }: { api: LoginApi; userId?: string; code?: string }) {
  const confirming = userId !== "" && code !== "";
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      if (confirming) await api.confirmPasswordReset({ userId, code, password });
      else await api.requestPasswordReset(email);
      setDone(true);
    } catch (err) {
      setError(messageOf(err));
    } finally {
      setBusy(false);
    }
  }

  if (done) {
    return (
      <LoginLayout title="Reset your password">
        <p className="text-sm">
          {confirming ? "Your password is set. Sign in from Cheladb Studio." : "If an account exists for that email, we've sent a reset link."}
        </p>
        <a href="/" className="text-sm underline">Back to Cheladb Studio</a>
      </LoginLayout>
    );
  }
  return (
    <LoginLayout title="Reset your password">
      <form onSubmit={submit} className="space-y-4">
        {confirming
          ? <Field label="New password" type="password" value={password} onChange={setPassword} autoComplete="new-password" />
          : <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />}
        {error && <Problem message={error} />}
        <Button type="submit" disabled={busy} className="w-full">{confirming ? "Set password" : "Send reset link"}</Button>
      </form>
    </LoginLayout>
  );
}
