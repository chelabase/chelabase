import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { useResendCode, useVerifyEmail } from "@/lib/login-client/generated/endpoints";
import { Field, LoginLayout, Problem, follow, problemOf, type FormProps, type LoginProblem } from "./sign-in-form";

export function VerifyEmailForm({ initialCode = "", toApp, toVerify, toPending }: FormProps) {
  const [code, setCode] = useState(initialCode);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<LoginProblem | null>(null);
  const [resent, setResent] = useState(false);
  const verifyEmail = useVerifyEmail();
  const resendCode = useResendCode();

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      follow(await verifyEmail.mutateAsync({ data: { code: code.trim() } }), { toApp, toVerify, toPending });
    } catch (err) {
      setError(problemOf(err));
      setBusy(false);
    }
  }

  async function resend() {
    await resendCode.mutateAsync({ data: {} }).catch(() => undefined);
    setResent(true);
  }

  return (
    <LoginLayout title="Check your email">
      <p className="text-sm text-muted-foreground">Enter the code we sent to your email.</p>
      <form onSubmit={submit} className="space-y-4">
        <Field label="Code" value={code} onChange={setCode} autoComplete="one-time-code" />
        {error && <Problem message={error.message} restart={error.restart} />}
        <Button type="submit" disabled={busy} className="w-full">Verify</Button>
      </form>
      <Button variant="outline" onClick={() => void resend()} className="w-full">Send a new code</Button>
      {resent && <p className="text-sm text-muted-foreground">If your account needs it, a new code is on its way.</p>}
    </LoginLayout>
  );
}
