import type { ReactNode } from "react";
import { useRailBehaviour } from "@/lib/rail-preference";
import { useMediaQuery } from "@/lib/use-media-query";
import { AppHeader } from "./app-header";
import { AppRail } from "./app-rail";
import type { RailBadges } from "./rail-items";

export interface AppShellProps {
  profile: { name?: string; email?: string };
  onSignOut: () => void;
  /** Counts on rail items (Members: sign-up requests waiting). */
  badges?: RailBadges;
  children: ReactNode;
}

/**
 * The signed-in frame, rendered once by the `/app` layout route: the rail on the
 * left, the header and the routed page on the right. Only `expanded` widens the
 * rail's column; in `hover` mode the rail overlays the page instead.
 */
export function AppShell({ profile, onSignOut, badges, children }: AppShellProps) {
  const [behaviour, setBehaviour] = useRailBehaviour();
  // Below 40rem an expanded rail opens over the page (see AppRail): its column stays narrow.
  const narrow = useMediaQuery("(max-width: 40rem)");
  const railWidth = behaviour === "expanded" && !narrow ? "232px" : "56px";
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
        <AppRail behaviour={behaviour} onBehaviourChange={setBehaviour} badges={badges} />
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
