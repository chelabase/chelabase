import type { UserManagerSettings } from "oidc-client-ts";
import type { StudioEnv } from "./env";

export function studioScopes(env: StudioEnv): string[] {
  return [
    "openid",
    "profile",
    "email",
    "offline_access",
    // Puts the user's org id in the access token; the gateway maps it to a tenant.
    "urn:zitadel:iam:user:resourceowner",
    // Adds the project to `aud`, matching the gateway's CHELADB_ZITADEL_AUDIENCES.
    `urn:zitadel:iam:org:project:id:${env.projectId}:aud`,
    // Pins sign-in and sign-up to the bootstrapped org (single-org mode).
    // Drop this to let each signup create its own organization.
    `urn:zitadel:iam:org:id:${env.orgId}`,
  ];
}

export function buildOidcConfig(env: StudioEnv, origin: string): UserManagerSettings {
  return {
    authority: env.issuer,
    client_id: env.clientId,
    redirect_uri: `${origin}/callback`,
    post_logout_redirect_uri: `${origin}/`,
    response_type: "code",
    scope: studioScopes(env).join(" "),
    automaticSilentRenew: true,
    loadUserInfo: true,
    revokeTokensOnSignout: true,
  };
}
