import { createFileRoute } from "@tanstack/react-router";
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
  }),
  component: AcceptInvitePage,
});

function AcceptInvitePage() {
  const { user, code, t, reset } = Route.useSearch();
  return (
    <LoginProviders>
      <AcceptInviteForm user={user} code={code} t={t} reset={reset} go={() => window.location.assign("/app")} />
    </LoginProviders>
  );
}
