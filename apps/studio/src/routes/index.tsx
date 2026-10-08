import { Navigate, createFileRoute } from "@tanstack/react-router";
import { useAuth } from "react-oidc-context";
import { SignIn } from "@/components/sign-in";

/** `/?signup` (a bare flag, from the landing page's "Start free") sends a signed-out visitor to account creation. */
export const validateSearch = (s: Record<string, unknown>): { signup?: true } => ("signup" in s ? { signup: true } : {});

export const Route = createFileRoute("/")({ validateSearch, component: Landing });

function Landing() {
  const auth = useAuth();
  const { signup } = Route.useSearch();
  if (auth.isAuthenticated) return <Navigate to="/app" replace />;
  return <SignIn auth={auth} signup={!!signup} />;
}
