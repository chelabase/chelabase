import { createFileRoute } from "@tanstack/react-router";
import { LoginProviders } from "@/components/login/login-providers";
import { PasswordResetForm } from "@/components/login/password-reset-form";

const str = (v: unknown) => (typeof v === "string" ? v : "");

export const Route = createFileRoute("/password-reset")({
  validateSearch: (s: Record<string, unknown>) => ({ userId: str(s.userId), code: str(s.code) }),
  component: PasswordResetPage,
});

function PasswordResetPage() {
  const { userId, code } = Route.useSearch();
  return (
    <LoginProviders>
      <PasswordResetForm userId={userId} code={code} />
    </LoginProviders>
  );
}
