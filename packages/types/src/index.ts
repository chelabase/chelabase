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

/** Stub control-plane API types (hand-written until orchestrator OpenAPI exists). */
export type DatabaseStatus =
  | "pending"
  | "provisioning"
  | "ready"
  | "failed"
  | "deleting";

export interface CreateDatabaseRequest {
  tenantId: TenantId;
  projectId: ProjectId;
  name: string;
}

export interface CreateDatabaseAccepted {
  jobId: string;
  databaseId: DatabaseId;
  status: "accepted";
}

export interface DatabaseSummary {
  id: DatabaseId;
  projectId: ProjectId;
  tenantId: TenantId;
  name: string;
  status: DatabaseStatus;
}
