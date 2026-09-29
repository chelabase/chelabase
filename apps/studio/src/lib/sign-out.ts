import type { LoginApi } from "./login-api";

/**
 * Sign out everywhere: auth's session cookie first (best effort), then Zitadel's
 * end-session redirect, which ends the Zitadel session and returns to Studio.
 */
export async function signOut(api: Pick<LoginApi, "logout">, redirect: () => Promise<void>): Promise<void> {
  await api.logout().catch(() => undefined);
  await redirect();
}
