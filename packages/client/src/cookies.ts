// The session cookies `@chelabase/client/server` reads and writes: names, attributes, parsing.

/** Overrides for the session cookies' attributes. */
export interface CookieOptions {
  /** Share the cookies with subdomains. Drops the `__Host-` prefix, which forbids `Domain`. */
  domain?: string;
  /** Default `"lax"`. */
  sameSite?: "lax" | "strict" | "none";
  /** Default `true`. `false` (plain-HTTP development only) drops the `__Host-` prefix. */
  secure?: boolean;
  /** Seconds. Default 400 days, for both cookies: the server enforces when the session ends. */
  maxAge?: number;
}

/** The attributes of one cookie, in the shape frameworks' cookie setters take. */
export interface CookieAttributes {
  path: "/";
  secure: boolean;
  httpOnly: boolean;
  sameSite: "lax" | "strict" | "none";
  maxAge: number;
  domain?: string;
}

export interface CookieToSet {
  name: string;
  value: string;
  options: CookieAttributes;
}

export interface SessionCookieNames {
  access: string;
  refresh: string;
}

const MAX_AGE = 400 * 86_400;

/**
 * `__Host-chelabase.<first 12 chars of key>.access` / `.refresh`: `__Host-` makes the browser
 * insist on `Secure; Path=/` and no `Domain`, so a sibling subdomain can't plant them. The
 * plain `chelabase.…` names only with `secure: false` or an explicit `domain`.
 */
export function cookieNames(key: string, o: CookieOptions = {}): SessionCookieNames {
  const prefix = o.secure === false || o.domain !== undefined ? "" : "__Host-";
  const base = `${prefix}chelabase.${key.slice(0, 12)}`;
  return { access: `${base}.access`, refresh: `${base}.refresh` };
}

/** Both cookies for a session, or both cleared (`Max-Age=0`) for `null`. The refresh cookie is `HttpOnly`. */
export function sessionCookies(
  names: SessionCookieNames,
  s: { access_token: string; refresh_token: string } | null,
  o: CookieOptions = {},
): CookieToSet[] {
  const attrs = (httpOnly: boolean): CookieAttributes => ({
    path: "/",
    secure: o.secure ?? true,
    httpOnly,
    sameSite: o.sameSite ?? "lax",
    maxAge: s === null ? 0 : (o.maxAge ?? MAX_AGE),
    ...(o.domain === undefined ? {} : { domain: o.domain }),
  });
  return [
    { name: names.access, value: s?.access_token ?? "", options: attrs(false) },
    { name: names.refresh, value: s?.refresh_token ?? "", options: attrs(true) },
  ];
}

/** The session's tokens from a request's cookies: the exact configured names only. */
export function readSession(
  list: { name: string; value: string }[],
  names: SessionCookieNames,
): { access: string | undefined; refresh: string | undefined } {
  const get = (name: string) => list.find((c) => c.name === name)?.value || undefined;
  return { access: get(names.access), refresh: get(names.refresh) };
}

/** A `Set-Cookie` header value. */
export function serializeCookie(c: CookieToSet): string {
  const o = c.options;
  let line = `${c.name}=${encodeURIComponent(c.value)}; Path=${o.path}; Max-Age=${o.maxAge}`;
  if (o.domain !== undefined) line += `; Domain=${o.domain}`;
  if (o.secure) line += "; Secure";
  if (o.httpOnly) line += "; HttpOnly";
  return `${line}; SameSite=${o.sameSite[0]!.toUpperCase()}${o.sameSite.slice(1)}`;
}

/** A `Cookie` request header's pairs, values decoded (left as is when not valid percent-encoding). */
export function parseCookies(header: string): { name: string; value: string }[] {
  const out: { name: string; value: string }[] = [];
  for (const part of header.split(";")) {
    const eq = part.indexOf("=");
    if (eq <= 0) continue;
    const name = part.slice(0, eq).trim();
    if (name === "") continue;
    const raw = part.slice(eq + 1).trim();
    let value = raw;
    try {
      value = decodeURIComponent(raw);
    } catch {
      // keep the raw value
    }
    out.push({ name, value });
  }
  return out;
}
