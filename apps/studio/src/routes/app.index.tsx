import { createFileRoute } from "@tanstack/react-router";
import { Dashboard } from "@/components/dashboard";
import { useAppSession } from "@/lib/app-session";

export const Route = createFileRoute("/app/")({ component: DashboardRoute });

function DashboardRoute() {
  const { onLoaded } = useAppSession();
  return <Dashboard onLoaded={onLoaded} />;
}
