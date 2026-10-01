import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { LoginProviders } from "@/components/login/login-providers";
import { SignUpForm } from "@/components/login/sign-up-form";

export const Route = createFileRoute("/register")({
  validateSearch: (search: Record<string, unknown>) => ({ authRequest: typeof search.authRequest === "string" ? search.authRequest : "" }),
  component: RegisterPage,
});

function RegisterPage() {
  const { authRequest } = Route.useSearch();
  const navigate = useNavigate();
  return (
    <LoginProviders>
      <SignUpForm authRequest={authRequest}
        go={(url) => window.location.assign(url)}
        toVerify={() => void navigate({ to: "/verify", search: { code: "" } })}
        toLogin={(notice) => void navigate({ to: "/login", search: { authRequest, notice }, replace: true })} />
    </LoginProviders>
  );
}
