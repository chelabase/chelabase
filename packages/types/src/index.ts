/** Branded string IDs mirroring crates/cheladb-types. */

export type TenantId = string & { readonly __brand: "TenantId" };
export type ProjectId = string & { readonly __brand: "ProjectId" };
export type DatabaseId = string & { readonly __brand: "DatabaseId" };
/** A main branch's id equals its database's id. */
export type BranchId = string & { readonly __brand: "BranchId" };

export function asTenantId(value: string): TenantId {
  return value as TenantId;
}

export function asProjectId(value: string): ProjectId {
  return value as ProjectId;
}

export function asDatabaseId(value: string): DatabaseId {
  return value as DatabaseId;
}

export function asBranchId(value: string): BranchId {
  return value as BranchId;
}

/** Control-plane API shapes (snake_case, as served by the orchestrator via the gateway). */
export type DatabaseStatus =
  | "pending"
  | "provisioning"
  | "ready"
  | "failed"
  | "deleting"
  | "deleted";

export interface MeUser {
  sub: string;
  org_id: string;
}

export interface DatabaseSummary {
  database_id: DatabaseId;
  name: string;
  status: DatabaseStatus;
  host: string | null;
  port: number | null;
  /** Live `normal` branches, main included (backups don't count). */
  branch_count: number;
}

export interface ProjectView {
  id: ProjectId;
  slug: string;
  databases: DatabaseSummary[];
}

export interface TenantView {
  id: TenantId;
  slug: string;
  projects: ProjectView[];
}

/** `tenant` is null when the caller's org isn't bound to a Cheladb tenant yet. */
export interface MeResponse {
  user: MeUser;
  tenant: TenantView | null;
}

export interface CreateDatabaseRequest {
  tenant_id: TenantId;
  project_id: ProjectId;
  name: string;
}

export interface CreateDatabaseAccepted {
  job_id: string;
  database_id: DatabaseId;
  status: DatabaseStatus;
}

/** Owner credentials of a ready database (`GET /v1/databases/{id}/connection`). */
export interface ConnectionDetails {
  host: string;
  port: number;
  database: string;
  user: string;
  password: string;
  uri: string;
  http_url: string;
}

export interface DatabaseDetail extends DataApiSettings {
  database_id: DatabaseId;
  project_id: ProjectId;
  tenant_id: TenantId;
  name: string;
  status: DatabaseStatus;
  host: string | null;
  port: number | null;
  error: string | null;
  /** Live `normal` branches, main included (backups don't count). */
  branch_count: number;
  /** Seconds of history kept. */
  history_window: number;
  max_branches: number;
}

export type BranchStatus =
  | "pending"
  | "provisioning"
  | "ready"
  | "resetting"
  | "failed"
  | "deleting"
  | "deleted";

/** `backup` branches are made by a restore and have no compute. */
export type BranchKind = "normal" | "backup";

/** `GET /v1/databases/{id}/branches` returns an array of these. `host` and `port` are set
 * only for a ready `normal` branch with an endpoint. */
export interface BranchSummary {
  id: BranchId;
  name: string;
  /** null only for main. */
  parent_branch_id: BranchId | null;
  kind: BranchKind;
  status: BranchStatus;
  host: string | null;
  port: number | null;
  /** What the user asked for, if a time. */
  branched_at_time: string | null;
  /** What it resolved to. */
  branched_at_lsn: string | null;
  created_at: string;
  /** Where the branch's history begins (RFC 3339); null when unknown. The earliest point
   * a branch can be made from, or restored to, is the later of this and now minus the window. */
  history_start: string | null;
  error: string | null;
}

/** Exactly one key: an RFC 3339 timestamp, or an LSN like `0/16B3748`. */
export type PointInTime = { timestamp: string } | { lsn: string };

/** `POST /v1/databases/{id}/branches`; the parent defaults to main, `at` to now. */
export interface CreateBranchRequest {
  name: string;
  parent_branch_id?: BranchId;
  at?: PointInTime;
}

/** `POST …/branches/{branch_id}/restore` */
export interface RestoreRequest {
  at: PointInTime;
}

/** The 202 answer of a restore: the branch is `resetting`, its old timeline becomes the backup. */
export interface RestoreAccepted {
  status: "resetting";
  backup_branch_id: BranchId;
  backup_name: string;
}

/** The data API's settings of a database. */
export interface DataApiSettings {
  /** 1 to 100000: rows one query returns at most. */
  max_rows: number;
  /** 100 to 120000. */
  statement_timeout_ms: number;
  /** 1 to 50: connections the data API holds per branch. */
  max_connections: number;
  /** `^[a-z_][a-z0-9_]{0,62}$`: the schema a query without one reads. */
  default_schema: string;
}

/** Answer of `PATCH /v1/databases/{id}`, and (as a `Partial`) its request. */
export interface DatabaseSettings extends DataApiSettings {
  /** Seconds, 3600 (1 hour) to 2592000 (30 days). */
  history_window: number;
  /** 0 to 20, counting live `normal` branches other than main. */
  max_branches: number;
}

export type UpdateDatabaseRequest = Partial<DatabaseSettings>;

// ---- Data API keys

export type DataKeyKind = "public" | "secret";

/** A live key of a branch (`GET .../branches/{branch_id}/keys`). */
export interface DataKey {
  id: string;
  kind: DataKeyKind;
  /** The Postgres role a request with this key runs as. */
  role: string;
  name: string;
  /** The first 12 characters of the key. */
  prefix: string;
  created_at: string;
  last_used_at: string | null;
  /** The whole key; only the public key's, which can be shown again. */
  key?: string;
}

/** A key just created or rotated; `key` is shown once (the public key's can be listed again). */
export interface CreatedDataKey {
  id: string;
  kind: DataKeyKind;
  role: string;
  name: string;
  prefix: string;
  key: string;
}

/** `POST .../keys`: a secret key. `role` defaults to `authenticated`. */
export interface CreateDataKeyRequest {
  /** 1 to 63 characters. */
  name: string;
  role?: string;
}

// ---- Data API schema (`GET /data/v1/schema`, and `.../data-api/schema` on the platform)

export interface DataApiSchema {
  version: string;
  /** The database's `default_schema` setting: where unqualified names resolve. */
  default_schema: string;
  schemas: Record<
    string,
    {
      tables: Record<string, DataApiTable>;
      functions: Record<string, DataApiFunction[]>;
      enums: Record<string, string[]>;
    }
  >;
}

export interface DataApiTable {
  kind: "table" | "view" | "materialized_view" | "foreign_table";
  rls: boolean;
  /** True only for a view with `security_invoker = true` (it reads with the caller's rights); false for every other kind. */
  security_invoker: boolean;
  /** `null` on `/data/v1/schema` (the key's own view); set on the platform route. */
  privileges: { anon: Priv[]; authenticated: Priv[] } | null;
  columns: Record<string, DataApiColumn>;
}

export type Priv = "select" | "insert" | "update" | "delete";

export interface DataApiColumn {
  type: string;
  array: boolean;
  nullable: boolean;
  has_default: boolean;
  generated: boolean;
  enum?: string;
}

export interface DataApiFunction {
  args: { name: string; type: string; array: boolean; has_default: boolean }[];
  returns: { type: string; array: boolean; set: boolean };
}
