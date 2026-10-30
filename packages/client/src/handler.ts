// `createAuthHandler`: the app route the browser client's session flows go through. Exported from
// `@chelabase/client/server`; the browser entry never imports it.
import { USER_HEADER } from "./auth.ts";
import {
  type CookieOptions,
  checkCookieOptions,
  cookieNames,
  parseCookies,
  readSession,
  serializeCookie,
  sessionCookies,
} from "./cookies.ts";
import { NO_REDIRECTS, toError } from "./error.ts";

export interface AuthHandlerOptions {
  /** The gateway's base URL. */
  url: string;
  /** The project's public (`chk_pub_…`) data key. */
  key: string;
  /**
   * The app's public URL. A request's `Origin` must equal its origin (or one of
   * `allowedOrigins`); `req.url` is never used for this, since behind a proxy it is internal.
   */
  siteUrl: string;
  /** More exact origins (`https://host[:port]`) allowed to call the route. No `*`, no `null`. */
  allowedOrigins?: string[];
  /** A secret (`chk_sec_…`) key; requires `clientIp`. Sent to the gateway only with an IP. */
  serverKey?: string;
  /**
   * The end user's IP for this request, from a header your platform sets and a client can't
   * forge (e.g. `x-vercel-forwarded-for`). Requires `serverKey`. No header is read by default.
   */
  clientIp?: (req: Request) => string | undefined;
  cookieOptions?: CookieOptions;
  /** Where the route is mounted. Default `/api/chelabase/auth`. */
  path?: string;
  /** Defaults to the global `fetch`. */
  fetch?: typeof fetch;
}

/** The flows the route serves; the upstream path is always one of these constants. */
const FLOWS = ["sign-in", "sign-up", "refresh", "change-password", "sign-out"] as const;
type Flow = (typeof FLOWS)[number];
/** Answers that end the session in the cookies. `refresh_token_invalid` isn't one: a stale request must not wipe a newer session. */
const CLEARS = new Set(["refresh_token_reused", "user_inactive", "user_session_revoked"]);

function originOf(u: string, what: string): string {
  let o: string;
  try {
    o = new URL(u).origin;
  } catch {
    throw new Error(`createAuthHandler: \`${what}\` must be an absolute http(s) URL`);
  }
  if (o === "null" || !/^https?:/.test(o)) throw new Error(`createAuthHandler: \`${what}\` must be an absolute http(s) URL`);
  return o;
}

/**
 * A web-standard handler for `POST {path}/{sign-in|sign-up|refresh|change-password|sign-out}`.
 * A session answer sets both cookies (the refresh cookie `HttpOnly`) and returns the session
 * without its `refresh_token`. Only a same-site `POST` with a JSON body is served: anything else
 * is refused before the gateway is asked.
 */
export function createAuthHandler(opts: AuthHandlerOptions): (req: Request) => Promise<Response> {
  // These messages never quote a key.
  if (!/^chk_pub_[0-9A-Za-z]+$/.test(opts.key)) {
    throw new Error("createAuthHandler: `key` must be the public (chk_pub_…) key; a secret key goes in `serverKey`");
  }
  if (opts.serverKey !== undefined && !/^chk_sec_[0-9A-Za-z]+$/.test(opts.serverKey)) {
    throw new Error("createAuthHandler: `serverKey` must be a secret (chk_sec_…) key");
  }
  if ((opts.serverKey === undefined) !== (opts.clientIp === undefined)) {
    throw new Error(
      `createAuthHandler: \`serverKey\` and \`clientIp\` go together; \`${opts.serverKey === undefined ? "serverKey" : "clientIp"}\` is missing`,
    );
  }
  checkCookieOptions(opts.cookieOptions);
  const origins = new Set([originOf(opts.siteUrl, "siteUrl")]);
  for (const o of opts.allowedOrigins ?? []) {
    // Exact origins only: `new URL(o).origin` must give back `o` itself (no path, no wildcard).
    let ok = false;
    try {
      ok = new URL(o).origin === o && /^https?:\/\/([0-9A-Za-z.-]+|\[[0-9A-Fa-f:.]+\])(:\d+)?$/.test(o);
    } catch {
      // not a URL
    }
    if (!ok) throw new Error("createAuthHandler: `allowedOrigins` entries must be exact origins like https://app.example.com (no `*`, no `null`)");
    origins.add(o);
  }
  const mount = `/${(opts.path ?? "/api/chelabase/auth").replace(/^\/+|\/+$/g, "")}/`;
  const names = cookieNames(opts.key, opts.cookieOptions);
  const base = opts.url.replace(/\/+$/, "");
  const doFetch = opts.fetch ?? globalThis.fetch.bind(globalThis);
  const { serverKey, clientIp } = opts;

  const json = (status: number, body: unknown, cookies: string[] = []) => {
    const headers = new Headers({ "cache-control": "no-store" });
    for (const c of cookies) headers.append("set-cookie", c);
    if (status === 204) return new Response(null, { status, headers });
    headers.set("content-type", "application/json");
    return new Response(JSON.stringify(body), { status, headers });
  };
  const fail = (status: number, code: string, message: string, cookies: string[] = []) =>
    json(status, { error: { code, message } }, cookies);
  const setSession = (s: { access_token: string; refresh_token: string } | null) =>
    sessionCookies(names, s, opts.cookieOptions).map(serializeCookie);

  return async (req) => {
    if (req.method !== "POST") {
      const res = fail(405, "method_not_allowed", "only POST is allowed");
      res.headers.set("allow", "POST");
      return res;
    }
    const origin = req.headers.get("origin");
    if (origin === null || !origins.has(origin)) return fail(403, "forbidden", "this origin may not call the auth route");
    const type = (req.headers.get("content-type") ?? "").split(";")[0]!.trim().toLowerCase();
    if (type !== "application/json") return fail(415, "unsupported_media_type", "the body must be application/json");
    // The raw path, not decoded: a flow is matched exactly, so any encoding is simply unknown.
    const path = new URL(req.url).pathname;
    const flow = path.startsWith(mount) ? path.slice(mount.length) : undefined;
    if (flow === undefined || !(FLOWS as readonly string[]).includes(flow)) return fail(404, "not_found", "no such auth flow");
    return run(req, flow as Flow);
  };

  async function run(req: Request, flow: Flow): Promise<Response> {
    const { access, refresh } = readSession(parseCookies(req.headers.get("cookie") ?? ""), names);
    const headers: Record<string, string> = {
      authorization: `Bearer ${opts.key}`,
      "content-type": "application/json",
      accept: "application/json",
    };
    const ip = clientIp?.(req);
    if (serverKey !== undefined && ip !== undefined && ip !== "") {
      headers["chelabase-server-key"] = serverKey;
      headers["chelabase-client-ip"] = ip;
    }

    let body: unknown;
    if (flow === "refresh" || flow === "sign-out") {
      // The refresh token comes only from the cookie; the request body is never read.
      if (refresh === undefined) {
        return flow === "sign-out"
          ? json(204, null, setSession(null))
          : fail(401, "refresh_token_invalid", "there is no session to refresh");
      }
      body = { refresh_token: refresh };
    } else {
      try {
        body = JSON.parse(await req.text());
      } catch {
        body = undefined;
      }
      if (typeof body !== "object" || body === null || Array.isArray(body)) {
        return fail(400, "invalid_request", "the body must be a JSON object");
      }
      if (flow === "change-password") {
        if (access === undefined) return fail(401, "user_token_invalid", "not signed in");
        headers[USER_HEADER] = access;
      }
    }

    // sign-out clears both cookies whatever the gateway answers.
    const always = flow === "sign-out" ? setSession(null) : [];
    let res: Response;
    let text: string;
    try {
      res = await doFetch(`${base}/auth/v1/app/${flow}`, {
        method: "POST",
        headers,
        body: JSON.stringify(body),
        redirect: NO_REDIRECTS,
      });
      text = await res.text();
    } catch {
      return fail(502, "unavailable", "the auth API could not be reached", always);
    }
    if (!res.ok) {
      const e = toError(res, text);
      const clear = always.length > 0 ? always : (flow === "refresh" || flow === "change-password") && CLEARS.has(e.code) ? setSession(null) : [];
      const envelope: Record<string, string> = { code: e.code, message: e.message };
      for (const k of ["detail", "hint"] as const) if (e[k] !== undefined) envelope[k] = e[k];
      return json(res.status, { error: envelope }, clear);
    }
    if (flow === "sign-out") return json(204, null, always);
    let out: unknown;
    try {
      out = text === "" ? null : JSON.parse(text);
    } catch {
      return fail(502, "internal", "the auth API answered with invalid JSON");
    }
    if (typeof out !== "object" || out === null) return json(res.status, out);
    const { refresh_token, ...rest } = out as Record<string, unknown>;
    if (typeof rest["access_token"] === "string" && typeof refresh_token === "string") {
      return json(res.status, rest, setSession({ access_token: rest["access_token"], refresh_token }));
    }
    return json(res.status, rest);
  }
}
