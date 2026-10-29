import { useRouterState } from "@tanstack/react-router";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { useRailBehaviour } from "@/lib/rail-preference";
import { NARROW_QUERY, useMediaQuery } from "@/lib/use-media-query";
import { AppHeader } from "./app-header";
import { AppRail } from "./app-rail";
import { CrumbLabelsProvider } from "./crumbs";
import { RailDrawer } from "./rail-drawer";
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
  // Below the breakpoint the rail isn't docked: it opens as a drawer from the header's menu button.
  const narrow = useMediaQuery(NARROW_QUERY);
  const [drawerOpen, setDrawerOpen] = useState(false);
  const menuButtonRef = useRef<HTMLButtonElement>(null);
  const drawerId = useId();
  const pathname = useRouterState({ select: (s) => s.location.pathname });
  const open = narrow && drawerOpen;
  // A navigation, or a resize to wide, closes the drawer.
  useEffect(() => {
    setDrawerOpen(false);
  }, [pathname, narrow]);
  const railWidth = behaviour === "expanded" ? "232px" : "56px";
  return (
    <CrumbLabelsProvider>
    <div
      className="grid min-h-screen bg-background text-foreground transition-[grid-template-columns] duration-200 ease-design motion-reduce:transition-none"
      style={{ gridTemplateColumns: narrow ? "1fr" : `${railWidth} 1fr` }}
    >
      <a
        href="#content"
        className="sr-only focus:not-sr-only focus:fixed focus:left-2 focus:top-2 focus:z-50 focus:rounded-md focus:bg-deep focus:px-3 focus:py-2 focus:text-sm focus:text-chalk focus:outline-2 focus:outline-offset-2 focus:outline-sand"
      >
        Skip to content
      </a>
      {/* The drawer stays mounted so a resize to wide closes it (and restores focus) instead of unmounting an open dialog. */}
      <RailDrawer open={open} onOpenChange={setDrawerOpen} id={drawerId} returnFocusRef={menuButtonRef}>
        <AppRail
          variant="drawer"
          behaviour={behaviour}
          onBehaviourChange={setBehaviour}
          badges={badges}
          onNavigate={() => setDrawerOpen(false)}
        />
      </RailDrawer>
      {!narrow && (
        <div className="sticky top-0 z-30 h-screen">
          <AppRail behaviour={behaviour} onBehaviourChange={setBehaviour} badges={badges} />
        </div>
      )}
      <div className="flex min-w-0 flex-col">
        <AppHeader
          profile={profile}
          onSignOut={onSignOut}
          onMenu={narrow ? () => setDrawerOpen((o) => !o) : undefined}
          menuOpen={open}
          menuControlsId={drawerId}
          menuButtonRef={menuButtonRef}
        />
        <main id="content" tabIndex={-1} className="min-w-0 flex-1 outline-none">
          {children}
        </main>
      </div>
    </div>
    </CrumbLabelsProvider>
  );
}
