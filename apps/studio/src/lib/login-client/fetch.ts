import { buildHeaders, requestJson, type ApiError } from "@cheladb/management-client";

export interface LoginClientConfig {
  baseUrl: string;
  fetchImpl?: typeof fetch;
}

/** Error type orval puts on the generated hooks. */
export type ErrorType<_E> = ApiError;

let config: LoginClientConfig | undefined;

export function configureLoginClient(next: LoginClientConfig): void {
  config = next;
}

/** Orval mutator for the login API: cookie auth, no bearer, no 401 hook. */
export const customFetch = async <T>(url: string, options: RequestInit): Promise<T> => {
  if (!config) throw new Error("Login client is not configured; call configureLoginClient first");
  const fetchImpl = config.fetchImpl ?? globalThis.fetch.bind(globalThis);
  return requestJson<T>(fetchImpl, `${config.baseUrl}${url}`, { ...options, headers: buildHeaders(options), credentials: "include" });
};
