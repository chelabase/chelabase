import { createFileRoute } from "@tanstack/react-router";
import { useEffect } from "react";
import { useStudioEnv } from "@/lib/env-context";
import { createLoginApi } from "@/lib/login-api";
import { safeRedirect } from "@/lib/safe-redirect";

export const Route = createFileRoute("/logout")({
  validateSearch: (s: Record<string, unknown>) => ({
    post_logout_redirect: typeof s.post_logout_redirect === "string" ? s.post_logout_redirect : "",
  }),
  component: LogoutPage,
});

function LogoutPage() {
  const { post_logout_redirect } = Route.useSearch();
  const env = useStudioEnv();
  useEffect(() => {
    const target = safeRedirect(post_logout_redirect, window.location.origin);
    // Leave even if auth is unreachable: the Zitadel tokens are already ended.
    void createLoginApi(env.apiUrl).logout().catch(() => undefined).finally(() => window.location.replace(target));
  }, [env.apiUrl, post_logout_redirect]);
  return <main className="p-8 text-muted-foreground">Signing you out…</main>;
}
