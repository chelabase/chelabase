import { createFileRoute } from "@tanstack/react-router";
import { useEffect } from "react";
import { logout } from "@/lib/login-client/generated/endpoints";
import { useLoginClient } from "@/lib/login-client/use-login-client";
import { safeRedirect } from "@/lib/safe-redirect";

export const Route = createFileRoute("/logout")({
  validateSearch: (s: Record<string, unknown>) => ({
    post_logout_redirect: typeof s.post_logout_redirect === "string" ? s.post_logout_redirect : "",
  }),
  component: LogoutPage,
});

function LogoutPage() {
  const { post_logout_redirect } = Route.useSearch();
  useLoginClient();
  useEffect(() => {
    const target = safeRedirect(post_logout_redirect, window.location.origin);
    // Leave even if auth is unreachable: the Zitadel tokens are already ended.
    void logout({}).catch(() => undefined).finally(() => window.location.replace(target));
  }, [post_logout_redirect]);
  return <main className="p-8 text-muted-foreground">Signing you out…</main>;
}
