/**
 * Generated Zitadel HTTP client (OpenAPI → orval).
 * Spec pin: v2.71.11 — see openapi/zitadel-v2.71.11.openapi.yaml
 */
export * from "./generated/zitadel";
export { customFetch } from "./mutator";

/** Zitadel release this package is generated against (matches Compose image tag). */
export const ZITADEL_VERSION = "v2.71.11" as const;
