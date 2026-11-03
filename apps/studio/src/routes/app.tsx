import { ApiError, configureManagementClient, useGetMe } from "@chelabase/management-client";
import { QueryClientProvider, type QueryClient } from "@tanstack/react-query";
import { Outlet, createFileRoute, useNavigate } from "@tanstack/react-router";
import { useEffect, useMemo, useState } from "react";
import { SignedInApp } from "@/components/app-shell/signed-in-app";
import { SignInError } from "@/components/sign-in-error";
import { Button } from "@/components/ui/button";
import { AccessTokenContext } from "@/lib/access-token";
import { useRegisterDevtoolsQueryClient } from "@/lib/devtools-query-client";
import { AppSessionContext, type AppSession } from "@/lib/app-session";
import { createStudioQueryClient } from "@/lib/query";
import { signOut } from "@/lib/sign-out";
import { useStudioEnv } from "@/lib/env-context";
import { useLoginClient } from "@/lib/login-client/use-login-client";
import { createReauthGuard, recoverFromRejectedToken, type ReauthGuard } from "@/lib/reauth";
import { createRealtimeClient } from "@/lib/realtime/client";
import { RealtimeContext } from "@/lib/realtime/context";
import type { RealtimeClient } from "@/lib/realtime/types";
import { realtimeAuthEvents, type Session } from "@/lib/session";
import { useSession, useSessionSnapshot } from "@/lib/session-context";

export const Route = createFileRoute("/app")({ component: AppPage });

/**
 * The tab's realtime client while signed in. It opens its socket only when a page first joins a
 * channel; every renewed token goes out as an `auth` frame, and signing out closes it for good (W32).
 */
function useRealtimeClient(apiUrl: string, session: Session, signedIn: boolean, token: string | undefined): RealtimeClient | null {
  const [client, setClient] = useState<RealtimeClient | null>(null);
  useEffect(() => {
    if (!signedIn) return;
    const c = createRealtimeClient({ apiUrl, token: session.token() });
    const detach = c.attachAuth(realtimeAuthEvents(session));
    setClient(c);
    return () => {
      detach();
      c.close();
      setClient(null);
    };
  }, [apiUrl, session, signedIn]);
  // A token stored before the listener was attached (the events cover every later renewal).
  useEffect(() => {
    if (client && token) client.setToken(token);
  }, [client, token]);
  return client;
}

/**
 * Points the management client at this Studio's API with the session's token. Runs during render
 * (not in an effect) so it's in place before the dashboard's first query: children's effects run
 * before ours. A 401 mints a fresh token once and runs the queries again; if `/token` refuses,
 * the session is over and `/login` follows (the session's status change takes the user there).
 */
function useConfigureManagementClient(apiUrl: string, session: Session, guard: ReauthGuard, queryClient: QueryClient): void {
  useMemo(() => {
    configureManagementClient({
      baseUrl: apiUrl,
      getToken: () => session.token(),
      onUnauthorized: recoverFromRejectedToken(guard, {
        refresh: () => session.refresh(),
        refreshed: () => void queryClient.invalidateQueries(),
        toLogin: () => session.clear(),
      }),
    });
  }, [apiUrl, session, guard, queryClient]);
}

function AppPage() {
  const env = useStudioEnv();
  const session = useSession();
  const { status, token } = useSessionSnapshot();
  const navigate = useNavigate();
  const [queryClient] = useState(createStudioQueryClient);
  useRegisterDevtoolsQueryClient(queryClient);
  // Sign-out ends auth's session through the login client.
  useLoginClient();
  const [guard] = useState(() => createReauthGuard(window.sessionStorage));

  useConfigureManagementClient(env.apiUrl, session, guard, queryClient);

  // Arriving without a token (a reload, or straight from sign-in): mint one from the cookie.
  const [checked, setChecked] = useState(status === "signed-in");
  const [startError, setStartError] = useState<string | null>(null);
  useEffect(() => {
    if (checked) return;
    session.start().then(
      () => setChecked(true),
      (err: unknown) => {
        setStartError(err instanceof Error ? err.message : "Couldn't reach Chelabase");
        setChecked(true);
      },
    );
  }, [checked, session]);
  // No session (never had one, it ended, or `/token` refused a refresh): sign in again.
  useEffect(() => {
    if (checked && status === "signed-out") void navigate({ to: "/login", search: { notice: "" }, replace: true });
  }, [checked, status, navigate]);

  const realtime = useRealtimeClient(env.apiUrl, session, status === "signed-in", token);
  const [signOutError, setSignOutError] = useState<string | null>(null);

  const appSession = useMemo<AppSession>(
    () => ({
      apiUrl: env.apiUrl,
      onLoaded: () => guard.reset(),
      // The header clears the query cache before this runs.
      onSignOut: () => {
        setSignOutError(null);
        signOut(session, {
          disconnect: () => realtime?.close(),
          leave: () => {
            queryClient.clear();
            void navigate({ to: "/login", search: { notice: "" }, replace: true });
          },
        }).catch(() =>
          setSignOutError("Couldn't sign you out: Chelabase didn't answer, so your session is still active. Try again."),
        );
      },
      profile: {},
    }),
    [env.apiUrl, guard, session, realtime, queryClient, navigate],
  );

  if (startError && status !== "signed-in") return <SignInError message={startError} />;
  if (status !== "signed-in") return <main className="p-8 text-muted-foreground">Signing you in…</main>;
  return (
    <QueryClientProvider client={queryClient}>
      {/* The token value (not a getter) so pages re-render when it refreshes. */}
      <AccessTokenContext.Provider value={token}>
        <RealtimeContext.Provider value={realtime}>
          <AppSessionContext.Provider value={appSession}>
            {signOutError && (
              <p role="alert" className="bg-background p-3 text-center text-sm text-destructive">
                {signOutError}
              </p>
            )}
            <SignedInLayout app={appSession} />
          </AppSessionContext.Provider>
        </RealtimeContext.Provider>
      </AccessTokenContext.Provider>
    </QueryClientProvider>
  );
}

/** The shell, once `/v1/me` has said who this is (it also gives the profile menu its name and email). */
function SignedInLayout({ app }: { app: AppSession }) {
  const me = useGetMe();
  const profile = useMemo(() => ({ name: me.data?.name ?? undefined, email: me.data?.email ?? undefined }), [me.data?.name, me.data?.email]);
  const withProfile = useMemo(() => ({ ...app, profile }), [app, profile]);
  // A valid token whose subject has no row in the tenant: nothing in Studio would work for them.
  if (me.data === undefined && me.error instanceof ApiError && me.error.status === 403) {
    return (
      <main className="flex min-h-screen items-center justify-center bg-background p-8 text-foreground">
        <div className="w-full max-w-sm space-y-4">
          <h1 className="text-2xl font-semibold tracking-tight">No access to this workspace</h1>
          <p className="text-sm">Your account isn't a member of this workspace. Ask a tenant admin to invite you.</p>
          <Button variant="outline" onClick={app.onSignOut}>
            Sign out
          </Button>
        </div>
      </main>
    );
  }
  return (
    <AppSessionContext.Provider value={withProfile}>
      <SignedInApp profile={profile} onSignOut={app.onSignOut}>
        <Outlet />
      </SignedInApp>
    </AppSessionContext.Provider>
  );
}
