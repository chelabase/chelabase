/** Studio configuration, written to the stack's `studio.env` by `chelabase dev bootstrap` (Compose passes it to Studio's container). */
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
