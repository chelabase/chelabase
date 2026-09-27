/** Fetch mutator used by orval-generated clients. */

export type CustomFetchOptions = RequestInit & {
  baseUrl?: string;
};

export const customFetch = async <T>(
  url: string,
  options: CustomFetchOptions = {},
): Promise<T> => {
  const baseUrl = options.baseUrl ?? "http://localhost:8080";
  const { baseUrl: _ignored, ...init } = options;
  const response = await fetch(`${baseUrl}${url}`, {
    ...init,
    headers: {
      Accept: "application/json",
      ...(init.headers ?? {}),
    },
  });

  if (!response.ok) {
    const body = await response.text().catch(() => "");
    throw new Error(`Zitadel request failed (${response.status}): ${body}`);
  }

  if (response.status === 204) {
    return undefined as T;
  }

  return (await response.json()) as T;
};
