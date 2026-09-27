import { Navigate, createFileRoute } from "@tanstack/react-router";
import { hasAuthParams, useAuth } from "react-oidc-context";
import { SignInError } from "@/components/sign-in-error";
import { callbackOutcome } from "@/lib/callback";

export const Route = createFileRoute("/callback")({ component: Callback });

function Callback() {
  const auth = useAuth();
  const outcome = callbackOutcome({
    isLoading: auth.isLoading,
    isAuthenticated: auth.isAuthenticated,
    error: auth.error,
    hasAuthParams: hasAuthParams(),
  });
  switch (outcome) {
    case "app":
      return <Navigate to="/app" replace />;
    case "home":
      return <Navigate to="/" replace />;
    case "error":
      return <SignInError message={auth.error?.message ?? "unknown error"} />;
    case "wait":
      return <main className="p-8 text-muted-foreground">Signing you in…</main>;
  }
}
