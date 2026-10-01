import { createFileRoute } from "@tanstack/react-router";
import { LoginProviders } from "@/components/login/login-providers";
import { VerifyEmailForm } from "@/components/login/verify-email-form";

export const Route = createFileRoute("/verify")({
  validateSearch: (search: Record<string, unknown>) => ({ code: typeof search.code === "string" ? search.code : "" }),
  component: VerifyPage,
});

function VerifyPage() {
  const { code } = Route.useSearch();
  return (
    <LoginProviders>
      <VerifyEmailForm initialCode={code}
        go={(url) => window.location.assign(url)}
        toVerify={() => undefined} />
    </LoginProviders>
  );
}
