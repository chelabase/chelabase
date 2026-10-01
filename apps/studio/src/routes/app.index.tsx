import { createFileRoute } from "@tanstack/react-router";
import { ApiNavLink } from "@/components/api-nav-link";
import { Dashboard } from "@/components/dashboard";
import { useAppSession } from "@/lib/app-session";

export const Route = createFileRoute("/app/")({ component: DashboardRoute });

function DashboardRoute() {
  const { apiUrl, profile, onLoaded, onSignOut } = useAppSession();
  return (
    <Dashboard
      apiUrl={apiUrl}
      profile={profile}
      onLoaded={onLoaded}
      onSignOut={onSignOut}
      nav={<ApiNavLink />}
    />
  );
}
