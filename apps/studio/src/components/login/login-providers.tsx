import { QueryClientProvider } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { useRegisterDevtoolsQueryClient } from "@/lib/devtools-query-client";
import { useLoginClient } from "@/lib/login-client/use-login-client";
import { createStudioQueryClient } from "@/lib/query";

/** The public sign-in pages: the login client configured and a Query provider for its mutations. */
export function LoginProviders({ children }: { children: ReactNode }) {
  useLoginClient();
  const [queryClient] = useState(createStudioQueryClient);
  useRegisterDevtoolsQueryClient(queryClient);
  return <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>;
}
