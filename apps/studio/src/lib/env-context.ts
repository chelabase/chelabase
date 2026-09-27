import { createContext, useContext } from "react";
import type { StudioEnv } from "./env";

export const EnvContext = createContext<StudioEnv | null>(null);

export function useStudioEnv(): StudioEnv {
  const env = useContext(EnvContext);
  if (!env) throw new Error("useStudioEnv must be used inside <EnvContext.Provider>");
  return env;
}
