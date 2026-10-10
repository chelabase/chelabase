import type { ReactNode } from "react";
import { usePendingRequestCount } from "@/components/members/pending-requests";
import { PendingGate } from "@/components/waiting-for-approval";
import { AppShell } from "./app-shell";

export interface SignedInAppProps {
  profile: { name?: string; email?: string };
  onSignOut: () => void;
  children: ReactNode;
}

/**
 * The `/app` layout's body: the waiting screen for a pending sign-up (R40), else the shell with
 * its rail badges (Members: sign-up requests waiting, for tenant admins) around the page.
 */
export function SignedInApp({ profile, onSignOut, children }: SignedInAppProps) {
  return (
    <PendingGate onSignOut={onSignOut}>
      <Shell profile={profile} onSignOut={onSignOut}>
        {children}
      </Shell>
    </PendingGate>
  );
}

function Shell({ profile, onSignOut, children }: SignedInAppProps) {
  const pending = usePendingRequestCount();
  return (
    <AppShell profile={profile} onSignOut={onSignOut} badges={{ members: pending }}>
      {children}
    </AppShell>
  );
}
