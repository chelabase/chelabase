// Email code routes: issue a code (emailed, or returned for link delivery when there is no
// SMTP), check one, and report SMTP status. Storage and its invariants: email/codes.ts.
//
// Imports from ../routes.ts are only called inside handlers (routes.ts imports this module).
import type { Caller } from "../config.ts";
import { CODE_KINDS, CODE_TTL_TEXT, type CodeKind } from "../email/codes.ts";
import { render } from "../email/templates.ts";
import { HttpError } from "../http.ts";
import {
  checkPasswordLength,
  context,
  ID_PATTERN,
  requireUser,
  userOut,
  type BaUser,
  type Context,
  type GlobalRoute,
  type Route,
} from "../routes.ts";

const DELIVERIES = ["email", "return"] as const;
const MAX_PASSWORD = 1024;
const MAX_PROJECT = 128;
const MAX_LINK = 2048;
const DEFAULT_PROJECT = "Chelabase";
const CODE_PATTERN = /^[0-9]{6}$/;
/** Control characters (C0, DEL, C1) and line/paragraph separators: never in a header or a link. */
const CONTROL = /[\u0000-\u001f\u007f-\u009f\u2028\u2029]/;

/** The kinds each caller may create. auth: all; orchestrator: invites (member invites). */
const CREATE_KINDS: Record<Caller, readonly CodeKind[]> = {
  auth: CODE_KINDS,
  orchestrator: ["invite"],
  worker: [],
};

function oneOf<T extends string>(body: Record<string, unknown>, key: string, values: readonly T[]): T {
  const v = body[key];
  if (typeof v !== "string" || !(values as readonly string[]).includes(v)) {
    throw new HttpError(400, "invalid_request", `${key} must be one of ${values.join(", ")}`);
  }
  return v as T;
}

function userId(body: Record<string, unknown>): string {
  const v = body.user_id;
  if (typeof v !== "string" || !ID_PATTERN.test(v)) throw new HttpError(400, "invalid_request", "user_id is invalid");
  return v;
}

function optText(body: Record<string, unknown>, key: string, max: number): string | undefined {
  const v = body[key];
  if (v === undefined) return undefined;
  if (typeof v !== "string" || v.trim() === "" || v.length > max || CONTROL.test(v)) {
    throw new HttpError(400, "invalid_request", `${key} must be a non-empty single-line string of at most ${max} characters`);
  }
  return v;
}

/** An absolute http(s) URL, as given. */
function optLink(body: Record<string, unknown>): string | undefined {
  const v = optText(body, "link", MAX_LINK);
  if (v === undefined) return undefined;
  let url: URL;
  try {
    url = new URL(v);
  } catch {
    throw new HttpError(400, "invalid_request", "link must be an absolute http or https URL");
  }
  if (url.protocol !== "https:" && url.protocol !== "http:") {
    throw new HttpError(400, "invalid_request", "link must be an absolute http or https URL");
  }
  return v;
}

/**
 * Sets the password after a reset or invite code: Better Auth's `setUserPassword` (the
 * `users.set_password` route) plus `revokeUserSessions`, and the email marked verified (the
 * code just proved it, as `resetPasswordEmailOTP` does).
 */
async function setPasswordAfterCode(ctx: Context, user: BaUser, password: string): Promise<BaUser> {
  const hashed = await ctx.password.hash(password);
  if (await ctx.internalAdapter.findCredentialAccount(user.id)) await ctx.internalAdapter.updatePassword(user.id, hashed);
  else await ctx.internalAdapter.createAccount({ userId: user.id, providerId: "credential", accountId: user.id, password: hashed });
  await ctx.internalAdapter.deleteUserSessions(user.id);
  if (user.emailVerified) return user;
  return (await ctx.internalAdapter.updateUser(user.id, { emailVerified: true })) as BaUser;
}

export function codeRoutes(): Route[] {
  return [
    {
      id: "codes.create",
      method: "POST",
      rest: ["codes"],
      async handle({ caller, mailer, registry, pool, body }) {
        const id = userId(body);
        const kind = oneOf(body, "kind", CODE_KINDS);
        const delivery = oneOf(body, "delivery", DELIVERIES);
        const project = optText(body, "project_name", MAX_PROJECT) ?? DEFAULT_PROJECT;
        const link = optLink(body);
        if (!CREATE_KINDS[caller].includes(kind)) {
          throw new HttpError(403, "forbidden", `${caller} may not create ${kind} codes`);
        }
        if (delivery === "email" && !mailer.configured) {
          throw new HttpError(409, "email_not_configured", "no SMTP server is configured; use delivery \"return\"");
        }
        return registry.use(pool, async (auth) => {
          const user = await requireUser(await context(auth), id);
          const { code, expiresAt } = await auth.codes.issue(user.id, kind);
          const out = { kind, delivery, expires_at: expiresAt.toISOString() };
          if (delivery === "return") return { status: 201, body: { ...out, code } };
          const email = render(kind, { project, code, expires: CODE_TTL_TEXT[kind], ...(link === undefined ? {} : { link }) });
          await mailer.send(user.email, email);
          return { status: 201, body: out };
        });
      },
    },
    {
      id: "codes.verify",
      method: "POST",
      rest: ["codes", "verify"],
      async handle({ registry, pool, body }) {
        const id = userId(body);
        const kind = oneOf(body, "kind", CODE_KINDS);
        const code = body.code;
        if (typeof code !== "string" || !CODE_PATTERN.test(code)) {
          throw new HttpError(400, "invalid_request", "code must be six digits");
        }
        const password = body.new_password;
        if (kind === "verify" && password !== undefined) {
          throw new HttpError(400, "invalid_request", "new_password is only for reset and invite codes");
        }
        if (kind !== "verify" && (typeof password !== "string" || password === "" || password.length > MAX_PASSWORD)) {
          throw new HttpError(400, "invalid_request", `new_password must be a non-empty string of at most ${MAX_PASSWORD} characters`);
        }
        return registry.use(pool, async (auth) => {
          const ctx = await context(auth);
          // Before the code is spent, so a weak password costs no attempt.
          if (typeof password === "string") checkPasswordLength(ctx, password);
          if (!(await auth.codes.consume(id, kind, code))) {
            throw new HttpError(400, "invalid_code", "the code is wrong, expired, used up or was replaced");
          }
          // The row cascades with its user, so the user existed when the code was consumed; a
          // concurrent delete since then is a plain 404.
          let user = await requireUser(ctx, id);
          if (kind === "verify") {
            if (!user.emailVerified) user = (await ctx.internalAdapter.updateUser(id, { emailVerified: true })) as BaUser;
          } else {
            user = await setPasswordAfterCode(ctx, user, password as string);
          }
          return { status: 200, body: { user: userOut(user) } };
        });
      },
    },
  ];
}

export function emailStatusRoute(): GlobalRoute {
  return {
    id: "email.status",
    method: "GET",
    path: "/internal/v1/email/status",
    async handle({ mailer }) {
      return { status: 200, body: { ...(await mailer.status()) } };
    },
  };
}
