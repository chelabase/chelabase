import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { LoginProviders } from "@/components/login/login-providers";
import { SignInForm } from "@/components/login/sign-in-form";

export const Route = createFileRoute("/login")({
  validateSearch: (search: Record<string, unknown>) => ({
    authRequest: typeof search.authRequest === "string" ? search.authRequest : "",
    notice: typeof search.notice === "string" ? search.notice : "",
  }),
  component: LoginPage,
});

function LoginPage() {
  const { authRequest, notice } = Route.useSearch();
  const navigate = useNavigate();
  return (
    <LoginProviders>
      <SignInForm authRequest={authRequest} notice={notice}
        go={(url) => window.location.assign(url)}
        toVerify={() => void navigate({ to: "/verify", search: { code: "" } })}
        toRegister={() => void navigate({ to: "/register", search: { authRequest }, replace: true })} />
    </LoginProviders>
  );
}
