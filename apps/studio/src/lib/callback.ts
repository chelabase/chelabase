export type CallbackOutcome = "wait" | "app" | "home" | "error";

/** Where the /callback page should go, given the sign-in state. */
export function callbackOutcome(state: {
  isLoading: boolean;
  isAuthenticated: boolean;
  error: Error | undefined;
  hasAuthParams: boolean;
}): CallbackOutcome {
  if (state.error) return "error";
  if (state.isAuthenticated) return "app";
  if (state.isLoading || state.hasAuthParams) return "wait";
  return "home";
}
