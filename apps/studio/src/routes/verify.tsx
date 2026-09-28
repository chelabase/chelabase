import { createFileRoute } from "@tanstack/react-router";
import { useMemo } from "react";
import { VerifyEmailForm } from "@/components/login/verify-email-form";
import { useStudioEnv } from "@/lib/env-context";
import { createLoginApi } from "@/lib/login-api";

export const Route = createFileRoute("/verify")({
  validateSearch: (search: Record<string, unknown>) => ({ code: typeof search.code === "string" ? search.code : "" }),
  component: VerifyPage,
});

function VerifyPage() {
  const { code } = Route.useSearch();
  const env = useStudioEnv();
  const api = useMemo(() => createLoginApi(env.apiUrl), [env.apiUrl]);
  return (
    <VerifyEmailForm api={api} initialCode={code}
      go={(url) => window.location.assign(url)}
      toVerify={() => undefined} />
  );
}
