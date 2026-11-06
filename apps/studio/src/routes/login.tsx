import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { LoginProviders } from "@/components/login/login-providers";
import { SignInForm } from "@/components/login/sign-in-form";

export const Route = createFileRoute("/login")({
  validateSearch: (search: Record<string, unknown>) => ({
    notice: typeof search.notice === "string" ? search.notice : "",
  }),
  component: LoginPage,
});

function LoginPage() {
  const { notice } = Route.useSearch();
  const navigate = useNavigate();
  return (
    <LoginProviders>
      <SignInForm notice={notice}
        toApp={() => void navigate({ to: "/app", replace: true })}
        toVerify={() => void navigate({ to: "/verify", search: { code: "" } })}
        toPending={() => void navigate({ to: "/pending", replace: true })}
        toRegister={() => void navigate({ to: "/register", replace: true })} />
    </LoginProviders>
  );
}
