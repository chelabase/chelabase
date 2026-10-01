import { logout } from "./login-client/generated/endpoints";

/**
 * Sign out everywhere: auth's session cookie first (best effort), then Zitadel's
 * end-session redirect, which ends the Zitadel session and returns to Studio.
 */
export async function signOut(redirect: () => Promise<void>): Promise<void> {
  await logout({}).catch(() => undefined);
  await redirect();
}
