import { Navigate, createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { SignIn } from "@/components/sign-in";
import { useSession, useSessionSnapshot } from "@/lib/session-context";

/** `/?signup` (a bare flag, from the landing page's "Start free") sends a signed-out visitor to account creation. */
export const validateSearch = (s: Record<string, unknown>): { signup?: true } => ("signup" in s ? { signup: true } : {});

export const Route = createFileRoute("/")({ validateSearch, component: Landing });

function Landing() {
  const session = useSession();
  const { status } = useSessionSnapshot();
  const { signup } = Route.useSearch();
  // An unreachable auth ends the check: the visitor still gets the page, and the sign-in form says why.
  const [unreachable, setUnreachable] = useState(false);
  useEffect(() => {
    if (status === "unknown") session.start().catch(() => setUnreachable(true));
  }, [session, status]);
  if (status === "signed-in") return <Navigate to="/app" replace />;
  if (status === "signed-out" && signup) return <Navigate to="/register" replace />;
  return <SignIn checking={status === "unknown" && !unreachable} />;
}
