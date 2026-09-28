import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useMemo } from "react";
import { SignInForm } from "@/components/login/sign-in-form";
import { useStudioEnv } from "@/lib/env-context";
import { createLoginApi } from "@/lib/login-api";

export const Route = createFileRoute("/login")({
  validateSearch: (search: Record<string, unknown>) => ({
    authRequest: typeof search.authRequest === "string" ? search.authRequest : "",
    notice: typeof search.notice === "string" ? search.notice : "",
  }),
  component: LoginPage,
});

function LoginPage() {
  const { authRequest, notice } = Route.useSearch();
  const env = useStudioEnv();
  const navigate = useNavigate();
  const api = useMemo(() => createLoginApi(env.apiUrl), [env.apiUrl]);
  return (
    <SignInForm api={api} authRequest={authRequest} notice={notice}
      go={(url) => window.location.assign(url)}
      toVerify={() => void navigate({ to: "/verify", search: { code: "" } })}
      toRegister={() => void navigate({ to: "/register", search: { authRequest }, replace: true })} />
  );
}
