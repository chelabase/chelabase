import type {
  CreateDatabaseAccepted,
  CreateDatabaseRequest,
  DatabaseDetail,
  MeResponse,
} from "@cheladb/types";

export class ApiError extends Error {
  readonly status: number;
  /** A 401 for which sign-in is being restarted (the page is about to leave). */
  readonly signInRestarted: boolean;

  constructor(status: number, message: string, signInRestarted = false) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.signInRestarted = signInRestarted;
  }
}

export interface ApiClientOptions {
  baseUrl: string;
  getToken: () => string | undefined;
  /**
   * Called on 401: the token was refused. Return true when sign-in is being
   * restarted, so callers can show that instead of an error.
   */
  onUnauthorized: () => boolean | void;
  fetchImpl?: typeof fetch;
}

export interface ApiClient {
  me(): Promise<MeResponse>;
  createDatabase(request: CreateDatabaseRequest): Promise<CreateDatabaseAccepted>;
  getDatabase(id: string): Promise<DatabaseDetail>;
}

export function createApiClient(options: ApiClientOptions): ApiClient {
  const fetchImpl = options.fetchImpl ?? globalThis.fetch.bind(globalThis);

  async function request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const headers: Record<string, string> = { Accept: "application/json" };
    const token = options.getToken();
    if (token) headers.Authorization = `Bearer ${token}`;
    if (body !== undefined) headers["Content-Type"] = "application/json";

    const response = await fetchImpl(`${options.baseUrl}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (response.status === 401) {
      const restarted = options.onUnauthorized() === true;
      throw new ApiError(401, await errorMessage(response), restarted);
    }
    if (!response.ok) throw new ApiError(response.status, await errorMessage(response));
    return (await response.json()) as T;
  }

  return {
    me: () => request<MeResponse>("GET", "/v1/me"),
    createDatabase: (req) => request<CreateDatabaseAccepted>("POST", "/v1/databases", req),
    getDatabase: (id) =>
      request<DatabaseDetail>("GET", `/v1/databases/${encodeURIComponent(id)}`),
  };
}

async function errorMessage(response: Response): Promise<string> {
  const text = await response.text().catch(() => "");
  try {
    const parsed = JSON.parse(text) as { error?: unknown };
    if (typeof parsed.error === "string") return parsed.error;
  } catch {
    // not JSON; fall through to the raw text
  }
  return text || `Request failed with status ${response.status}`;
}
