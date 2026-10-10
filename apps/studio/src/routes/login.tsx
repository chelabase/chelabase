import { Navigate, createFileRoute, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { LoginProviders } from "@/components/login/login-providers";
import { SignInForm } from "@/components/login/sign-in-form";
import { resumeInvite } from "@/lib/pending-invite";

export const Route = createFileRoute("/login")({
  validateSearch: (search: Record<string, unknown>) => ({
    authRequest: typeof search.authRequest === "string" ? search.authRequest : "",
    notice: typeof search.notice === "string" ? search.notice : "",
  }),
  component: LoginPage,
});

function LoginPage() {
  const { authRequest, notice } = Route.useSearch();
  const navigate = useNavigate();
  // An invite link started this sign-in: finish it on the accept page instead.
  const [resume] = useState(() => resumeInvite(authRequest));
  if (resume) return <Navigate to="/accept-invite" search={{ user: resume.user, code: "", t: "", reset: false, authRequest: resume.authRequest }} replace />;
  return (
    <LoginProviders>
      <SignInForm authRequest={authRequest} notice={notice}
        go={(url) => window.location.assign(url)}
        toVerify={() => void navigate({ to: "/verify", search: { code: "" } })}
        toRegister={() => void navigate({ to: "/register", search: { authRequest }, replace: true })} />
    </LoginProviders>
  );
}
