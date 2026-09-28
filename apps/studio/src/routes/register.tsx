import { createFileRoute, useNavigate } from "@tanstack/react-router";
import { useMemo } from "react";
import { SignUpForm } from "@/components/login/sign-up-form";
import { useStudioEnv } from "@/lib/env-context";
import { createLoginApi } from "@/lib/login-api";

export const Route = createFileRoute("/register")({
  validateSearch: (search: Record<string, unknown>) => ({ authRequest: typeof search.authRequest === "string" ? search.authRequest : "" }),
  component: RegisterPage,
});

function RegisterPage() {
  const { authRequest } = Route.useSearch();
  const env = useStudioEnv();
  const navigate = useNavigate();
  const api = useMemo(() => createLoginApi(env.apiUrl), [env.apiUrl]);
  return (
    <SignUpForm api={api} authRequest={authRequest}
      go={(url) => window.location.assign(url)}
      toVerify={() => void navigate({ to: "/verify", search: { code: "" } })}
      toLogin={(notice) => void navigate({ to: "/login", search: { authRequest, notice }, replace: true })} />
  );
}
