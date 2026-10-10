import { useEffect, useRef } from "react";
import type { AuthContextProps } from "react-oidc-context";
import { Button } from "@/components/ui/button";

export interface SignInProps {
  auth: Pick<AuthContextProps, "isLoading" | "error" | "signinRedirect">;
  /** `/?signup`: go straight to the account-creation screen, once per mount. */
  signup: boolean;
}

export function SignIn({ auth, signup }: SignInProps) {
  const redirected = useRef(false);
  const { isLoading, error, signinRedirect } = auth;

  useEffect(() => {
    if (!signup || isLoading || error || redirected.current) return;
    redirected.current = true;
    // "replace": Back from Zitadel must land before this page, not on it (which would redirect again)
    void signinRedirect({ prompt: "create", redirectMethod: "replace" });
  }, [signup, isLoading, error, signinRedirect]);

  return (
    <main className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background p-8 text-foreground">
      <h1 className="text-3xl font-semibold tracking-tight">Chelabase Studio</h1>
      <p className="max-w-md text-center text-muted-foreground">Postgres databases for your projects.</p>
      <div className="flex gap-3">
        <Button disabled={isLoading} onClick={() => void signinRedirect()}>
          Sign in
        </Button>
      </div>
      {error && (
        <p role="alert" className="text-sm text-destructive">
          Sign-in failed: {error.message}
        </p>
      )}
    </main>
  );
}
