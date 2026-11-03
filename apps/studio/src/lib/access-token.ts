import { createContext, useContext } from "react";

/** The signed-in user's current member token; changes when it refreshes. */
export const AccessTokenContext = createContext<string | undefined>(undefined);

export function useAccessToken(): string | undefined {
  return useContext(AccessTokenContext);
}
