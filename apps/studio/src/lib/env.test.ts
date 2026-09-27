import { describe, expect, it } from "vitest";
import { readEnv } from "./env";

const full = {
  VITE_ZITADEL_ISSUER: "http://localhost:8080",
  VITE_ZITADEL_CLIENT_ID: "client-1",
  VITE_ZITADEL_PROJECT_ID: "project-1",
  VITE_ZITADEL_ORG_ID: "org-1",
  VITE_API_URL: "http://127.0.0.1:8000",
};

describe("readEnv", () => {
  it("maps every VITE_ setting", () => {
    expect(readEnv(full)).toEqual({
      issuer: "http://localhost:8080",
      clientId: "client-1",
      projectId: "project-1",
      orgId: "org-1",
      apiUrl: "http://127.0.0.1:8000",
    });
  });

  it("names every missing setting and how to fix it", () => {
    expect(() => readEnv({ ...full, VITE_ZITADEL_CLIENT_ID: "", VITE_API_URL: undefined })).toThrow(
      /VITE_ZITADEL_CLIENT_ID, VITE_API_URL.*just zitadel-bootstrap/,
    );
  });
});
