import { createContext, useContext } from "react";
import type { RealtimeClient } from "./types";

/** The tab's one socket client (null: no realtime, e.g. in tests that don't provide one). */
export const RealtimeContext = createContext<RealtimeClient | null>(null);

export function useRealtime(): RealtimeClient | null {
  return useContext(RealtimeContext);
}
