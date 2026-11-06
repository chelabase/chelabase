import { logout } from "./login-client/generated/endpoints";
import { ApiError } from "@chelabase/management-client";

export interface SignOutParts {
  /** Closes the realtime socket. */
  disconnect: () => void;
  /** Leaves the signed-in app (to `/login`). */
  leave: () => void;
}

/**
 * Ends the cookie session, then drops the in-memory token, closes realtime and leaves. When auth
 * can't end the session (it answers 503 and keeps the cookie, or is unreachable) this throws and
 * changes nothing: the person is still signed in and must be told so. A 401 means there was no
 * session left to end, which is a sign-out too.
 */
export async function signOut(session: { clear: () => void }, { disconnect, leave }: SignOutParts): Promise<void> {
  try {
    await logout({});
  } catch (err) {
    if (!(err instanceof ApiError && err.status === 401)) throw err;
  }
  session.clear();
  disconnect();
  leave();
}
