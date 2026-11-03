import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { LoginProviders } from "@/components/login/login-providers";
import { VerifyEmailForm } from "@/components/login/verify-email-form";

export const Route = createFileRoute("/verify")({
  validateSearch: (search: Record<string, unknown>) => ({ code: typeof search.code === "string" ? search.code : "" }),
  component: VerifyPage,
});

function VerifyPage() {
  const { code } = Route.useSearch();
  const navigate = useNavigate();
  return (
    <LoginProviders>
      <VerifyEmailForm initialCode={code}
        toApp={() => void navigate({ to: "/app", replace: true })}
        toVerify={() => undefined}
        toPending={() => void navigate({ to: "/pending", replace: true })} />
    </LoginProviders>
  );
}
