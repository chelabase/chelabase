import { createFileRoute } from "@tanstack/react-router";
import { useMemo } from "react";
import { PasswordResetForm } from "@/components/login/password-reset-form";
import { useStudioEnv } from "@/lib/env-context";
import { createLoginApi } from "@/lib/login-api";

const str = (v: unknown) => (typeof v === "string" ? v : "");

export const Route = createFileRoute("/password-reset")({
  validateSearch: (s: Record<string, unknown>) => ({ userId: str(s.userId), code: str(s.code) }),
  component: PasswordResetPage,
});

function PasswordResetPage() {
  const { userId, code } = Route.useSearch();
  const env = useStudioEnv();
  const api = useMemo(() => createLoginApi(env.apiUrl), [env.apiUrl]);
  return <PasswordResetForm api={api} userId={userId} code={code} />;
}
