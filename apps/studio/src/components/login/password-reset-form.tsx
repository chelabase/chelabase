import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { useConfirmPasswordReset, useRequestPasswordReset } from "@/lib/login-client/generated/endpoints";
import { Field, LoginLayout, Problem, SECRET, messageOf } from "./sign-in-form";

type Step = "request" | "confirm" | "done";

/**
 * `/password-reset`: ask for a code by email, then set the new password with it. The email
 * links back here and carries no code or id, so someone coming back from it enters the email
 * with the code. A confirmed reset ends every session of the account.
 */
export function PasswordResetForm() {
  const [step, setStep] = useState<Step>("request");
  // Set when this page sent the code: the confirm step then knows the email.
  const [sent, setSent] = useState(false);
  const [email, setEmail] = useState("");
  const [code, setCode] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestReset = useRequestPasswordReset();
  const confirmReset = useConfirmPasswordReset(SECRET);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      if (step === "confirm") {
        await confirmReset.mutateAsync({ data: { email: email.trim(), code: code.trim(), password } });
        setPassword("");
        setStep("done");
      } else {
        await requestReset.mutateAsync({ data: { email: email.trim() } });
        setSent(true);
        setStep("confirm");
      }
    } catch (err) {
      setError(messageOf(err));
    } finally {
      confirmReset.reset();
      requestReset.reset();
      setBusy(false);
    }
  }

  if (step === "done") {
    return (
      <LoginLayout title="Reset your password">
        <p className="text-sm">Your password is set, and you're signed out everywhere. Sign in with the new one.</p>
        <a href="/login" className="text-sm underline">Sign in</a>
      </LoginLayout>
    );
  }
  if (step === "confirm") {
    return (
      <LoginLayout title="Reset your password">
        {sent && <p className="text-sm text-muted-foreground">If an account exists for that email, we've sent it a code.</p>}
        <form onSubmit={submit} className="space-y-4">
          {!sent && <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />}
          <Field label="Code" value={code} onChange={setCode} autoComplete="one-time-code" />
          <Field label="New password" type="password" value={password} onChange={setPassword} autoComplete="new-password" />
          {error && <Problem message={error} />}
          <Button type="submit" disabled={busy} className="w-full">Set password</Button>
        </form>
      </LoginLayout>
    );
  }
  return (
    <LoginLayout title="Reset your password">
      <form onSubmit={submit} className="space-y-4">
        <Field label="Email" type="email" value={email} onChange={setEmail} autoComplete="username" />
        {error && <Problem message={error} />}
        <Button type="submit" disabled={busy} className="w-full">Send a code</Button>
      </form>
      <Button type="button" variant="ghost" className="w-full" onClick={() => { setError(null); setStep("confirm"); }}>
        I have a code
      </Button>
    </LoginLayout>
  );
}
