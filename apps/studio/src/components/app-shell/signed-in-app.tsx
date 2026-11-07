import type { ReactNode } from "react";
import { useGetMe } from "@chelabase/management-client";
import { usePendingRequestCount } from "@/components/members/pending-requests";
import { PendingGate } from "@/components/waiting-for-approval";
import { AppShell } from "./app-shell";
import { useFunctionsUnavailable } from "@/lib/functions";
import { hiddenRailItems } from "./rail-items";

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
  const me = useGetMe();
  const functionsUnavailable = useFunctionsUnavailable(!!me.data?.member);
  return (
    <AppShell profile={profile} onSignOut={onSignOut} badges={{ members: pending }} hidden={hiddenRailItems(me.data, { functionsUnavailable })}>
      {children}
    </AppShell>
  );
}
