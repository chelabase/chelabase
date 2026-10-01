import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useMemo, useRef } from "react";
import { useAuth } from "react-oidc-context";
import { Dashboard } from "@/components/dashboard";
import { SignInError } from "@/components/sign-in-error";
import { createApiClient } from "@/lib/api";
import { createLoginApi } from "@/lib/login-api";
import { signOut } from "@/lib/sign-out";
import { useStudioEnv } from "@/lib/env-context";
import { createReauthGuard, resetOnLoad } from "@/lib/reauth";

export const Route = createFileRoute("/app")({ component: AppPage });

function AppPage() {
  const auth = useAuth();
  const env = useStudioEnv();
  // A ref keeps the API client stable while tokens refresh underneath it.
  const authRef = useRef(auth);
  authRef.current = auth;

  const api = useMemo(() => {
    const guard = createReauthGuard(window.sessionStorage);
    const client = createApiClient({
      baseUrl: env.apiUrl,
      getToken: () => authRef.current.user?.access_token,
      // Restart sign-in once; if a fresh token is refused too, let the
      // dashboard show the gateway's error instead of looping through Zitadel.
      onUnauthorized: () => {
        if (!guard.shouldRedirect()) return false;
        void authRef.current.signinRedirect();
        return true;
      },
    });
    return resetOnLoad(client, guard);
  }, [env.apiUrl]);

  const needsSignIn = !auth.isLoading && !auth.isAuthenticated && !auth.activeNavigator && !auth.error;
  useEffect(() => {
    if (needsSignIn) void auth.signinRedirect();
  }, [needsSignIn, auth]);

  if (auth.error && !auth.isAuthenticated) return <SignInError message={auth.error.message} />;
  if (!auth.isAuthenticated) {
    return <main className="p-8 text-muted-foreground">Redirecting to sign in…</main>;
  }
  return (
    <Dashboard
      api={api}
      apiUrl={env.apiUrl}
      profile={{ name: auth.user?.profile.name, email: auth.user?.profile.email }}
      onSignOut={() => void signOut(createLoginApi(env.apiUrl), () => auth.signoutRedirect())}
    />
  );
}
