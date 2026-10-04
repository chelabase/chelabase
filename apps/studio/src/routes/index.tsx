import { Navigate, createFileRoute } from "@tanstack/react-router";
import { useAuth } from "react-oidc-context";
import { Button } from "@/components/ui/button";

export const Route = createFileRoute("/")({ component: Landing });

function Landing() {
  const auth = useAuth();
  if (auth.isAuthenticated) return <Navigate to="/app" replace />;
  return (
    <main className="flex min-h-screen flex-col items-center justify-center gap-6 bg-background p-8 text-foreground">
      <h1 className="text-3xl font-semibold tracking-tight">Chelabase Studio</h1>
      <p className="max-w-md text-center text-muted-foreground">Postgres databases for your projects.</p>
      <div className="flex gap-3">
        <Button disabled={auth.isLoading} onClick={() => void auth.signinRedirect()}>
          Sign in
        </Button>
        <Button
          variant="outline"
          disabled={auth.isLoading}
          onClick={() => void auth.signinRedirect({ prompt: "create" })}
        >
          Create account
        </Button>
      </div>
      {auth.error && (
        <p role="alert" className="text-sm text-destructive">
          Sign-in failed: {auth.error.message}
        </p>
      )}
    </main>
  );
}
