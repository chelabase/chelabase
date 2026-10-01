import { configureManagementClient } from "@cheladb/management-client";
import { QueryClientProvider } from "@tanstack/react-query";
import { Outlet, createFileRoute } from "@tanstack/react-router";
import { useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { useAuth, type AuthContextProps } from "react-oidc-context";
import { SignInError } from "@/components/sign-in-error";
import { AccessTokenContext } from "@/lib/access-token";
import { useRegisterDevtoolsQueryClient } from "@/lib/devtools-query-client";
import { AppSessionContext, type AppSession } from "@/lib/app-session";
import { createStudioQueryClient } from "@/lib/query";
import { signOut } from "@/lib/sign-out";
import { useStudioEnv } from "@/lib/env-context";
import { useLoginClient } from "@/lib/login-client/use-login-client";
import { createReauthGuard, restartSignInOnce, type ReauthGuard } from "@/lib/reauth";

export const Route = createFileRoute("/app")({ component: AppPage });

/**
 * Points the management client at this Studio's API with the signed-in user's token.
 * Runs during render (not in an effect) so it's in place before the dashboard's
 * first query: children's effects run before ours. Re-runs only when the URL changes.
 */
function useConfigureManagementClient(apiUrl: string, authRef: RefObject<AuthContextProps>, guard: ReauthGuard): void {
  useMemo(() => {
    configureManagementClient({
      baseUrl: apiUrl,
      getToken: () => authRef.current.user?.access_token,
      // Restart sign-in once; if a fresh token is refused too, let the
      // dashboard show the gateway's error instead of looping through Zitadel.
      onUnauthorized: restartSignInOnce(guard, () => void authRef.current.signinRedirect()),
    });
  }, [apiUrl, authRef, guard]);
}

function AppPage() {
  const auth = useAuth();
  const env = useStudioEnv();
  // A ref keeps the clients stable while tokens refresh underneath them.
  const authRef = useRef(auth);
  authRef.current = auth;
  const [queryClient] = useState(createStudioQueryClient);
  useRegisterDevtoolsQueryClient(queryClient);
  // Sign-out ends auth's session through the login client.
  useLoginClient();
  const [guard] = useState(() => createReauthGuard(window.sessionStorage));

  useConfigureManagementClient(env.apiUrl, authRef, guard);

  const name = auth.user?.profile.name;
  const email = auth.user?.profile.email;
  const session = useMemo<AppSession>(
    () => ({
      apiUrl: env.apiUrl,
      profile: { name, email },
      onLoaded: () => guard.reset(),
      // The dashboard clears the query cache before this runs.
      onSignOut: () => void signOut(() => authRef.current.signoutRedirect()),
    }),
    [env.apiUrl, name, email, guard],
  );

  const needsSignIn = !auth.isLoading && !auth.isAuthenticated && !auth.activeNavigator && !auth.error;
  useEffect(() => {
    if (needsSignIn) void auth.signinRedirect();
  }, [needsSignIn, auth]);

  if (auth.error && !auth.isAuthenticated) return <SignInError message={auth.error.message} />;
  if (!auth.isAuthenticated) {
    return <main className="p-8 text-muted-foreground">Redirecting to sign in…</main>;
  }
  return (
    <QueryClientProvider client={queryClient}>
      {/* The token value (not a getter) so pages re-render when it refreshes. */}
      <AccessTokenContext.Provider value={auth.user?.access_token}>
        <AppSessionContext.Provider value={session}>
          <Outlet />
        </AppSessionContext.Provider>
      </AccessTokenContext.Provider>
    </QueryClientProvider>
  );
}
