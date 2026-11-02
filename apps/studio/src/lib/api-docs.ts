import loginDocument from "../../../../openapi/login.json";

/** The three public APIs the API reference page documents, in tab order. */
export const API_DOCS = [
  { id: "management", label: "Management API" },
  { id: "data", label: "Data API" },
  { id: "auth", label: "Auth API" },
] as const;

export type ApiDocId = (typeof API_DOCS)[number]["id"];

export interface ApiSearch {
  doc?: Exclude<ApiDocId, "management">;
}

/** `?doc=data|auth`; Management, the default, has no search param. */
export function parseApiSearch(search: Record<string, unknown>): ApiSearch {
  return search.doc === "data" || search.doc === "auth" ? { doc: search.doc } : {};
}

/**
 * The login document minus Studio's own sign-in endpoints: only `/auth/v1/app/*`, the app-user
 * auth that apps call, is a public contract.
 */
export function appUserAuthDocument() {
  const paths = Object.fromEntries(Object.entries(loginDocument.paths).filter(([p]) => p.startsWith("/auth/v1/app/")));
  return { ...loginDocument, info: { ...loginDocument.info, title: "Chelabase Auth API" }, paths };
}
