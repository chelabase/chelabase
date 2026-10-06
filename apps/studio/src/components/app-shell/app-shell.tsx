import type { ReactNode } from "react";
import { useRailBehaviour } from "@/lib/rail-preference";
import { AppHeader } from "./app-header";
import { AppRail } from "./app-rail";

export interface AppShellProps {
  profile: { name?: string; email?: string };
  onSignOut: () => void;
  children: ReactNode;
}

/**
 * The signed-in frame, rendered once by the `/app` layout route: the rail on the
 * left, the header and the routed page on the right. Only `expanded` widens the
 * rail's column; in `hover` mode the rail overlays the page instead.
 */
export function AppShell({ profile, onSignOut, children }: AppShellProps) {
  const [behaviour, setBehaviour] = useRailBehaviour();
  const railWidth = behaviour === "expanded" ? "232px" : "56px";
  return (
    <div
      className="grid min-h-screen bg-background text-foreground transition-[grid-template-columns] duration-200 ease-lagoon motion-reduce:transition-none"
      style={{ gridTemplateColumns: `${railWidth} 1fr` }}
    >
      <a
        href="#content"
        className="sr-only focus:not-sr-only focus:fixed focus:left-2 focus:top-2 focus:z-50 focus:rounded-md focus:bg-deep focus:px-3 focus:py-2 focus:text-sm focus:text-chalk focus:outline-2 focus:outline-offset-2 focus:outline-sand"
      >
        Skip to content
      </a>
      <div className="sticky top-0 z-30 h-screen">
        <AppRail behaviour={behaviour} onBehaviourChange={setBehaviour} />
      </div>
      <div className="flex min-w-0 flex-col">
        <AppHeader profile={profile} onSignOut={onSignOut} />
        <main id="content" tabIndex={-1} className="min-w-0 flex-1 outline-none">
          {children}
        </main>
      </div>
    </div>
  );
}
