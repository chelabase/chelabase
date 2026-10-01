import { createContext, useContext } from "react";

/** What the signed-in layout (routes/app.tsx) hands to its child routes. */
export interface AppSession {
  apiUrl: string;
  profile: { name?: string; email?: string };
  onLoaded: () => void;
  onSignOut: () => void;
}

export const AppSessionContext = createContext<AppSession | null>(null);

export function useAppSession(): AppSession {
  const session = useContext(AppSessionContext);
  if (!session) throw new Error("useAppSession must be used inside the signed-in layout");
  return session;
}
