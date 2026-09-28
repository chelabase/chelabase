import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { EXPIRED, Field, LoginLayout, Problem, follow, messageOf, type FormProps } from "./sign-in-form";

export function VerifyEmailForm({ api, initialCode = "", go, toVerify }: FormProps) {
  const [code, setCode] = useState(initialCode);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [resent, setResent] = useState(false);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await api.verifyEmail(code.trim()), go, toVerify);
    } catch (err) {
      setError(messageOf(err));
      setBusy(false);
    }
  }

  async function resend() {
    await api.resendCode().catch(() => undefined);
    setResent(true);
  }

  return (
    <LoginLayout title="Check your email">
      <p className="text-sm text-muted-foreground">Enter the code we sent to your email.</p>
      <form onSubmit={submit} className="space-y-4">
        <Field label="Code" value={code} onChange={setCode} autoComplete="one-time-code" />
        {error && <Problem message={error} restart={error === EXPIRED} />}
        <Button type="submit" disabled={busy} className="w-full">Verify</Button>
      </form>
      <Button variant="outline" onClick={() => void resend()} className="w-full">Send a new code</Button>
      {resent && <p className="text-sm text-muted-foreground">If your account needs it, a new code is on its way.</p>}
    </LoginLayout>
  );
}
