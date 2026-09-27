import { describe, expect, it } from "vitest";
import { buildOidcConfig, studioScopes } from "./auth";

const env = {
  issuer: "http://localhost:8080",
  clientId: "client-1",
  projectId: "project-1",
  orgId: "org-1",
  apiUrl: "http://127.0.0.1:8000",
};

describe("studioScopes", () => {
  it("asks for the org claim, the project audience and pins the org", () => {
    expect(studioScopes(env)).toEqual(
      expect.arrayContaining([
        "openid",
        "profile",
        "email",
        "offline_access",
        "urn:zitadel:iam:user:resourceowner",
        "urn:zitadel:iam:org:project:id:project-1:aud",
        "urn:zitadel:iam:org:id:org-1",
      ]),
    );
  });
});

describe("buildOidcConfig", () => {
  it("uses PKCE code flow with redirects on the Studio origin", () => {
    const config = buildOidcConfig(env, "http://localhost:3000");
    expect(config.authority).toBe("http://localhost:8080");
    expect(config.client_id).toBe("client-1");
    expect(config.response_type).toBe("code");
    expect(config.redirect_uri).toBe("http://localhost:3000/callback");
    expect(config.post_logout_redirect_uri).toBe("http://localhost:3000/");
    expect(config.scope).toContain("urn:zitadel:iam:org:id:org-1");
  });

  it("revokes tokens on sign-out so a refresh token can't outlive the session", () => {
    expect(buildOidcConfig(env, "http://localhost:3000").revokeTokensOnSignout).toBe(true);
  });
});
