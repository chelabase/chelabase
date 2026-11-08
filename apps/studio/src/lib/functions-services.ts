import { buildHeaders, getGetServiceLogsUrl, managementConnection, requestJson, type Function as FunctionInfo, type FunctionDeployment } from "@chelabase/management-client";

/** A service deployment's spec, read defensively: the API types it as an open object. */
export interface ServiceSpecView {
  backend: string;
  image: string | null;
  url: string | null;
  port: number | null;
  health_path: string | null;
}

const str = (v: unknown) => (typeof v === "string" ? v : null);

/** The spec of a service deployment; null for a function's (no spec) or one without a backend. */
export function serviceSpecOf(spec: unknown): ServiceSpecView | null {
  if (typeof spec !== "object" || spec === null || Array.isArray(spec)) return null;
  const s = spec as Record<string, unknown>;
  if (typeof s.backend !== "string") return null;
  return {
    backend: s.backend,
    image: str(s.image),
    url: str(s.url),
    port: typeof s.port === "number" ? s.port : null,
    health_path: str(s.health_path),
  };
}

/** What a service deployment runs: its image (a digest once deployed) or its URL. */
export function deploymentTarget(d: Pick<FunctionDeployment, "spec">): string | null {
  const spec = serviceSpecOf(d.spec);
  return spec ? (spec.image ?? spec.url) : null;
}

export type StatusTone = "good" | "bad" | "dim";

/**
 * A service's state on the branch, from its switch (the pending value when a flip is in flight),
 * its active deployment and the last health check. Null for a function.
 */
export function serviceStatus(fn: Pick<FunctionInfo, "kind" | "enabled" | "deployment_id" | "health">, enabled: boolean = fn.enabled): { label: string; tone: StatusTone } | null {
  if (fn.kind !== "service") return null;
  if (!enabled) return { label: "Stopped", tone: "dim" };
  if (fn.deployment_id === null) return { label: "Not deployed", tone: "dim" };
  // A switch just turned on hasn't been checked yet, whatever the last check said.
  if (!fn.enabled || fn.health === null) return { label: "Starting", tone: "dim" };
  return fn.health === "healthy" ? { label: "Running", tone: "good" } : { label: "Unhealthy", tone: "bad" };
}

/**
 * A container service's last 200 log lines, as text. The generated `getServiceLogs` parses every
 * answer as JSON, which a plain-text body isn't, so this reads it as text.
 */
export async function fetchServiceLogs(databaseId: string, branchId: string, name: string, fetchImpl: typeof fetch = (input, init) => globalThis.fetch(input, init)): Promise<string> {
  const conn = managementConnection();
  const init: RequestInit = { method: "GET" };
  const headers = buildHeaders(init, conn.token ? { Authorization: `Bearer ${conn.token}` } : {});
  headers.set("Accept", "text/plain, application/json");
  const text = await requestJson<string | undefined>(fetchImpl, `${conn.baseUrl}${getGetServiceLogsUrl(databaseId, branchId, name)}`, { ...init, headers }, conn.onUnauthorized, (t) => t);
  return text ?? "";
}
