/** Branded string IDs mirroring crates/cheladb-types. */

export type TenantId = string & { readonly __brand: "TenantId" };
export type ProjectId = string & { readonly __brand: "ProjectId" };
export type DatabaseId = string & { readonly __brand: "DatabaseId" };

export function asTenantId(value: string): TenantId {
  return value as TenantId;
}

export function asProjectId(value: string): ProjectId {
  return value as ProjectId;
}

export function asDatabaseId(value: string): DatabaseId {
  return value as DatabaseId;
}

/** Control-plane API shapes (snake_case, as served by the orchestrator via the gateway). */
export type DatabaseStatus =
  | "pending"
  | "provisioning"
  | "ready"
  | "failed"
  | "deleting";

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

export interface DatabaseDetail {
  database_id: DatabaseId;
  project_id: ProjectId;
  tenant_id: TenantId;
  name: string;
  status: DatabaseStatus;
  host: string | null;
  port: number | null;
  error: string | null;
}
