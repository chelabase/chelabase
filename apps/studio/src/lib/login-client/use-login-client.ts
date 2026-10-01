import { useMemo } from "react";
import { useStudioEnv } from "@/lib/env-context";
import { configureLoginClient } from "./fetch";

/**
 * Points the login client at this Studio's API URL. Runs during render (not in
 * an effect) so it's in place before a child's first call; idempotent.
 */
export function useLoginClient(): void {
  const env = useStudioEnv();
  useMemo(() => configureLoginClient({ baseUrl: env.apiUrl }), [env.apiUrl]);
}
