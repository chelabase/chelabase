import { createFileRoute } from "@tanstack/react-router";
import { AcceptInviteForm } from "@/components/login/accept-invite-form";
import { LoginProviders } from "@/components/login/login-providers";

const str = (v: unknown) => (typeof v === "string" ? v : typeof v === "number" ? String(v) : "");

export const Route = createFileRoute("/accept-invite")({
  validateSearch: (s: Record<string, unknown>) => ({
    user: str(s.user),
    // Only an admin's link carries the code; the invite email has it in its text.
    code: str(s.code),
    t: str(s.t),
  }),
  component: AcceptInvitePage,
});

function AcceptInvitePage() {
  const { user, code, t } = Route.useSearch();
  return (
    <LoginProviders>
      <AcceptInviteForm user={user} code={code} t={t} go={(url) => window.location.assign(url)} />
    </LoginProviders>
  );
}
