import type {
  BranchSummary,
  ConnectionDetails,
  CreateBranchRequest,
  CreateDataKeyRequest,
  CreatedDataKey,
  CreateDatabaseAccepted,
  CreateDatabaseRequest,
  DataApiSchema,
  DataKey,
  DatabaseDetail,
  DatabaseSettings,
  MeResponse,
  RestoreAccepted,
  RestoreRequest,
  UpdateDatabaseRequest,
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
  /** Owner credentials; only for ready databases. */
  getConnection(id: string): Promise<ConnectionDetails>;
  resetPassword(id: string): Promise<void>;
  /** Starts deletion (202); the database then shows `deleting` until gone. */
  deleteDatabase(id: string): Promise<void>;
  /** History window (seconds) and branch cap. */
  updateDatabase(id: string, request: UpdateDatabaseRequest): Promise<DatabaseSettings>;
  /** A bare array; `parent_branch_id` makes the tree. */
  listBranches(databaseId: string): Promise<BranchSummary[]>;
  /** 202: the new branch is `provisioning` until its compute is up. */
  createBranch(databaseId: string, request: CreateBranchRequest): Promise<BranchSummary>;
  /** Owner credentials of a ready `normal` branch. */
  getBranchConnection(databaseId: string, branchId: string): Promise<ConnectionDetails>;
  resetBranchPassword(databaseId: string, branchId: string): Promise<void>;
  /** 202: the branch is `resetting` until it matches its parent again. */
  resetBranchToParent(databaseId: string, branchId: string): Promise<void>;
  /** 202: the branch is `resetting`; its old data becomes the backup branch. */
  restoreBranch(databaseId: string, branchId: string, request: RestoreRequest): Promise<RestoreAccepted>;
  /** 202: the branch (and its backups) show `deleting` until gone. */
  deleteBranch(databaseId: string, branchId: string): Promise<void>;
  /** The branch's live data API keys; the public key comes in full. */
  listKeys(databaseId: string, branchId: string): Promise<DataKey[]>;
  /** A secret key; `key` is returned this once. A 422 carries the worker's reason. */
  createKey(databaseId: string, branchId: string, request: CreateDataKeyRequest): Promise<CreatedDataKey>;
  /** The new key replaces the old one, which stops working within 30 seconds. */
  rotateKey(databaseId: string, branchId: string, keyId: string): Promise<CreatedDataKey>;
  /** Secret keys only; the public key can only be rotated (409). */
  revokeKey(databaseId: string, branchId: string, keyId: string): Promise<void>;
  /** Every table `anon` or `authenticated` can reach, with its RLS flag. */
  getDataApiSchema(databaseId: string, branchId: string): Promise<DataApiSchema>;
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
    const text = await response.text();
    return (text ? JSON.parse(text) : undefined) as T;
  }

  const databasePath = (id: string) => `/v1/databases/${encodeURIComponent(id)}`;

  const branchesPath = (id: string) => `${databasePath(id)}/branches`;
  const branchPath = (id: string, branchId: string) => `${branchesPath(id)}/${encodeURIComponent(branchId)}`;

  return {
    me: () => request<MeResponse>("GET", "/v1/me"),
    createDatabase: (req) => request<CreateDatabaseAccepted>("POST", "/v1/databases", req),
    getDatabase: (id) => request<DatabaseDetail>("GET", databasePath(id)),
    getConnection: (id) => request<ConnectionDetails>("GET", `${databasePath(id)}/connection`),
    resetPassword: async (id) => {
      await request<unknown>("POST", `${databasePath(id)}/reset-password`);
    },
    deleteDatabase: async (id) => {
      await request<unknown>("DELETE", databasePath(id));
    },
    updateDatabase: (id, req) => request<DatabaseSettings>("PATCH", databasePath(id), req),
    listBranches: (id) => request<BranchSummary[]>("GET", branchesPath(id)),
    createBranch: (id, req) => request<BranchSummary>("POST", branchesPath(id), req),
    getBranchConnection: (id, branchId) =>
      request<ConnectionDetails>("GET", `${branchPath(id, branchId)}/connection`),
    resetBranchPassword: async (id, branchId) => {
      await request<unknown>("POST", `${branchPath(id, branchId)}/reset-password`);
    },
    resetBranchToParent: async (id, branchId) => {
      await request<unknown>("POST", `${branchPath(id, branchId)}/reset-to-parent`);
    },
    restoreBranch: (id, branchId, req) =>
      request<RestoreAccepted>("POST", `${branchPath(id, branchId)}/restore`, req),
    deleteBranch: async (id, branchId) => {
      await request<unknown>("DELETE", branchPath(id, branchId));
    },
    listKeys: (id, branchId) => request<DataKey[]>("GET", `${branchPath(id, branchId)}/keys`),
    createKey: (id, branchId, req) => request<CreatedDataKey>("POST", `${branchPath(id, branchId)}/keys`, req),
    rotateKey: (id, branchId, keyId) =>
      request<CreatedDataKey>("POST", `${branchPath(id, branchId)}/keys/${encodeURIComponent(keyId)}/rotate`),
    revokeKey: async (id, branchId, keyId) => {
      await request<unknown>("DELETE", `${branchPath(id, branchId)}/keys/${encodeURIComponent(keyId)}`);
    },
    getDataApiSchema: (id, branchId) =>
      request<DataApiSchema>("GET", `${branchPath(id, branchId)}/data-api/schema`),
  };
}

async function errorMessage(response: Response): Promise<string> {
  const text = await response.text().catch(() => "");
  try {
    const parsed = JSON.parse(text) as { error?: unknown };
    if (typeof parsed.error === "string") return parsed.error;
    // The data API nests it: { error: { code, message } }.
    const nested = (parsed.error as { message?: unknown } | null | undefined)?.message;
    if (typeof nested === "string") return nested;
  } catch {
    // not JSON; fall through to the raw text
  }
  return text || `Request failed with status ${response.status}`;
}
