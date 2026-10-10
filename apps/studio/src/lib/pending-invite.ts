/**
 * An invite link opened without a pending sign-in (the normal case: a link from an email). The
 * accept page keeps the link here (this tab only, 10 minutes) while it starts the sign-in that
 * Zitadel sends back to `/login`; `/login` sees it and returns to the accept page, which takes
 * it (and so removes it). The code and token never go back into an address.
 */
export interface PendingInvite {
  user: string;
  code: string;
  t: string;
  reset: boolean;
}

const KEY = "chelabase.pending-invite";
const TTL_MS = 10 * 60 * 1000;

interface Stored extends PendingInvite {
  at: number;
}

function read(): Stored | null {
  try {
    const raw = window.sessionStorage.getItem(KEY);
    if (!raw) return null;
    const v = JSON.parse(raw) as Partial<Stored>;
    if (typeof v.user !== "string" || typeof v.code !== "string" || typeof v.t !== "string" || typeof v.at !== "number") {
      return null;
    }
    if (Date.now() - v.at > TTL_MS) {
      window.sessionStorage.removeItem(KEY);
      return null;
    }
    return { user: v.user, code: v.code, t: v.t, reset: v.reset === true, at: v.at };
  } catch {
    return null;
  }
}

export function savePendingInvite(invite: PendingInvite): void {
  try {
    window.sessionStorage.setItem(KEY, JSON.stringify({ ...invite, at: Date.now() }));
  } catch {
    // Storage refused: the accept page can't resume after sign-in, and says so.
  }
}

/** Whose invite is waiting, without its code: what `/login` needs to send the visitor back. */
export function peekPendingInvite(): { user: string } | null {
  const v = read();
  return v ? { user: v.user } : null;
}

/** The waiting invite for `user`, removed as it's read. */
export function takePendingInvite(user: string): PendingInvite | null {
  const v = read();
  if (!v || v.user !== user) return null;
  try {
    window.sessionStorage.removeItem(KEY);
  } catch {
    // Nothing to do.
  }
  return { user: v.user, code: v.code, t: v.t, reset: v.reset };
}

/**
 * What `/login` hands back to the accept page when this tab started a sign-in for an invite:
 * the user and the pending sign-in, never the code (it stays in storage). Null otherwise.
 */
export function resumeInvite(authRequest: string): { user: string; authRequest: string } | null {
  if (authRequest === "") return null;
  const pending = peekPendingInvite();
  return pending ? { user: pending.user, authRequest } : null;
}

export function clearPendingInvite(): void {
  try {
    window.sessionStorage.removeItem(KEY);
  } catch {
    // Nothing to do.
  }
}
