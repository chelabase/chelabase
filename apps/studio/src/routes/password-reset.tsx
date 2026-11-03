import { createFileRoute } from "@tanstack/react-router";
import { LoginProviders } from "@/components/login/login-providers";
import { PasswordResetForm } from "@/components/login/password-reset-form";

export const Route = createFileRoute("/password-reset")({
  component: PasswordResetPage,
});

function PasswordResetPage() {
  return (
    <LoginProviders>
      <PasswordResetForm />
    </LoginProviders>
  );
}
