import type { ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { Button } from "@/components/ui/button";

export interface StudioShellProps {
  profile: { name?: string; email?: string };
  onSignOut: () => void;
  /** Extra header links, shown before "Sign out". */
  nav?: ReactNode;
  children: ReactNode;
}

/** The signed-in page frame: header with the profile, nav links and Sign out. */
export function StudioShell({ profile, onSignOut, nav, children }: StudioShellProps) {
  const queryClient = useQueryClient();
  return (
    <div className="min-h-screen bg-background text-foreground">
      <header className="flex items-center justify-between border-b px-8 py-4">
        <Link to="/app" className="font-semibold">
          Cheladb Studio
        </Link>
        <div className="flex items-center gap-4 text-sm">
          <span className="flex flex-col items-end leading-tight">
            {profile.name && <span>{profile.name}</span>}
            {profile.email && <span className="text-muted-foreground">{profile.email}</span>}
          </span>
          {nav}
          <Button
            variant="outline"
            size="sm"
            onClick={() => {
              // Nothing of this session may outlive it in the cache.
              queryClient.clear();
              onSignOut();
            }}
          >
            Sign out
          </Button>
        </div>
      </header>
      <main className="mx-auto max-w-4xl space-y-8 p-8">{children}</main>
    </div>
  );
}
