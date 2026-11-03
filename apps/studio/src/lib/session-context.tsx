import { createContext, useContext, useSyncExternalStore, type ReactNode } from "react";
import type { Session, SessionSnapshot } from "./session";

const SessionContext = createContext<Session | null>(null);

/** Hands the tab's one session (the in-memory member token) to the routes. */
export function SessionProvider({ session, children }: { session: Session; children: ReactNode }) {
  return <SessionContext.Provider value={session}>{children}</SessionContext.Provider>;
}

export function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) throw new Error("useSession must be used inside SessionProvider");
  return session;
}

/** The session's status and token, re-rendering when either changes. */
export function useSessionSnapshot(): SessionSnapshot {
  const session = useSession();
  return useSyncExternalStore(session.subscribe, session.snapshot);
}
