/**
 * Studio configuration. A self-hosted Studio reads it at run time from auth's
 * `GET /auth/v1/studio-config` on its own origin ({@link loadEnv}), so one image works for every
 * install. The workbench's Vite server answers that path 404, and Studio uses the `VITE_*` build
 * values from the stack's `studio.env` instead ({@link readEnv}).
 */
export interface StudioEnv {
  issuer: string;
  clientId: string;
  projectId: string;
  orgId: string;
  apiUrl: string;
}

const KEYS: Record<keyof StudioEnv, string> = {
  issuer: "VITE_ZITADEL_ISSUER",
  clientId: "VITE_ZITADEL_CLIENT_ID",
  projectId: "VITE_ZITADEL_PROJECT_ID",
  orgId: "VITE_ZITADEL_ORG_ID",
  apiUrl: "VITE_API_URL",
};

export function readEnv(source: Record<string, string | undefined>): StudioEnv {
  const value = (key: keyof StudioEnv) => source[KEYS[key]]?.trim() ?? "";
  const missing = (Object.keys(KEYS) as (keyof StudioEnv)[])
    .filter((key) => value(key) === "")
    .map((key) => KEYS[key]);
  if (missing.length > 0) {
    throw new Error(
      `Studio is missing ${missing.join(", ")}. Run \`just zitadel-bootstrap\` to write the stack's studio.env, then \`just workbench restart studio\`.`,
    );
  }
  return {
    issuer: value("issuer"),
    clientId: value("clientId"),
    projectId: value("projectId"),
    orgId: value("orgId"),
    apiUrl: value("apiUrl"),
  };
}

/** Where auth serves Studio's runtime config, on Studio's own origin (the gateway routes it). */
export const STUDIO_CONFIG_PATH = "/auth/v1/studio-config";

const RUNTIME_KEYS: (keyof StudioEnv)[] = ["issuer", "clientId", "projectId", "orgId", "apiUrl"];

/** The runtime config's values; anything missing, blank or not a string is refused, by name. */
export function readRuntimeConfig(body: unknown): StudioEnv {
  const record = (body && typeof body === "object" ? body : {}) as Record<string, unknown>;
  const value = (key: keyof StudioEnv) => {
    const v = record[key];
    return typeof v === "string" ? v.trim() : "";
  };
  const missing = RUNTIME_KEYS.filter((key) => value(key) === "");
  if (missing.length > 0) {
    throw new Error(`Studio's runtime config (${STUDIO_CONFIG_PATH}) is missing ${missing.join(", ")}.`);
  }
  return {
    issuer: value("issuer"),
    clientId: value("clientId"),
    projectId: value("projectId"),
    orgId: value("orgId"),
    apiUrl: value("apiUrl"),
  };
}

/**
 * Studio's config: auth's runtime config when it serves one, else (a 404: the workbench's Vite
 * server) the `VITE_*` build values in `source`. Anything else fails: a 503 while the bootstrap
 * hasn't run, an unreachable auth or a malformed answer never falls back to build values.
 */
export async function loadEnv(
  source: Record<string, string | undefined>,
  fetchImpl: (url: string, init?: RequestInit) => Promise<Response> = (url, init) => globalThis.fetch(url, init),
): Promise<StudioEnv> {
  let res: Response;
  try {
    res = await fetchImpl(STUDIO_CONFIG_PATH, {
      headers: { accept: "application/json" },
      credentials: "omit",
    });
  } catch (err) {
    throw new Error(
      `Studio couldn't load its config from ${STUDIO_CONFIG_PATH}: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
  if (res.status === 404) return readEnv(source);
  if (res.status === 503) {
    throw new Error("This instance is still setting up (its bootstrap hasn't finished). Reload in a moment.");
  }
  if (!res.ok) {
    throw new Error(`Studio couldn't load its config from ${STUDIO_CONFIG_PATH} (HTTP ${res.status}).`);
  }
  if (!(res.headers.get("content-type") ?? "").includes("application/json")) {
    throw new Error(`Studio's config from ${STUDIO_CONFIG_PATH} isn't JSON.`);
  }
  let body: unknown;
  try {
    body = await res.json();
  } catch {
    throw new Error(`Studio's config from ${STUDIO_CONFIG_PATH} isn't JSON.`);
  }
  return readRuntimeConfig(body);
}
