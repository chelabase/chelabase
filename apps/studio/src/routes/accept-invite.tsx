import { createFileRoute } from "@tanstack/react-router";
import { useCallback } from "react";
import { useAuth } from "react-oidc-context";
import { AcceptInviteForm } from "@/components/login/accept-invite-form";
import { LoginProviders } from "@/components/login/login-providers";

const str = (v: unknown) => (typeof v === "string" ? v : "");

export const Route = createFileRoute("/accept-invite")({
  validateSearch: (s: Record<string, unknown>) => ({
    user: str(s.user),
    code: str(s.code),
    t: str(s.t),
    // `reset=1` (a password-reset code) may arrive as the number 1 or the string "1".
    reset: s.reset === 1 || s.reset === "1",
    // Set when `/login` sends the visitor back after the sign-in this page started.
    authRequest: str(s.authRequest),
  }),
  component: AcceptInvitePage,
});

function AcceptInvitePage() {
  const { user, code, t, reset, authRequest } = Route.useSearch();
  const auth = useAuth();
  // "replace": Back from Zitadel must not land on this page again (it would start another).
  const startSignIn = useCallback(() => void auth.signinRedirect({ redirectMethod: "replace" }), [auth]);
  return (
    <LoginProviders>
      <AcceptInviteForm
        user={user}
        code={code}
        t={t}
        reset={reset}
        authRequest={authRequest}
        startSignIn={startSignIn}
        go={(url) => window.location.assign(url)}
      />
    </LoginProviders>
  );
}
