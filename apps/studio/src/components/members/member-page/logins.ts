import type { DatabaseSummary, PersonalLoginRevokeReason, PersonalLoginView } from "@chelabase/management-client";

/** A Postgres login name Chelabase accepts (`role_name::check_login_role_name`). */
export const LOGIN_NAME = /^[a-z_][a-z0-9_]{0,62}$/;

export const LOGIN_NAME_RULE = "Use lowercase letters, digits and underscores, starting with a letter or an underscore, up to 63 characters.";

/** The default login name: the email's local part, lowercased, anything else an underscore, never starting with a digit. */
export function defaultLoginName(email: string | null): string {
  const local = (email ?? "").split("@")[0].toLowerCase();
  let name = local.replace(/[^a-z0-9_]+/g, "_");
  if (name === "") name = "login";
  if (/^[0-9]/.test(name)) name = `_${name}`;
  return name.slice(0, 63);
}

/**
 * The connection string with `options=-c role=<group>` (P33), so a session starts as the group and
 * what it creates is owned by it. The value is URL-encoded; a space or backslash in the name is
 * backslash-escaped first (libpq splits `options` on spaces).
 */
export function withDefaultRole(connectionString: string, role: string): string {
  if (role === "") return connectionString;
  const value = role.replace(/[\\ ]/g, "\\$&");
  return `${connectionString}${connectionString.includes("?") ? "&" : "?"}options=-c%20role%3D${encodeURIComponent(value)}`;
}

/** A connection string for the database's main branch without a password: the client prompts for it. */
export function passwordlessString(login: Pick<PersonalLoginView, "role_name">, db: Pick<DatabaseSummary, "host" | "port" | "postgres_database"> | undefined): string | null {
  if (!db || !db.host || db.port === null || !db.postgres_database) return null;
  return `postgresql://${encodeURIComponent(login.role_name)}@${db.host}:${db.port}/${encodeURIComponent(db.postgres_database)}?sslmode=verify-full`;
}

const DAY = 86_400_000;
/** A login this close to its end offers Renew. */
export const RENEW_WITHIN_DAYS = 14;

export type LoginPhase = "revoked" | "expired" | "soon" | "ok";

export function phaseOf(l: Pick<PersonalLoginView, "status" | "valid_until">, now: number = Date.now()): LoginPhase {
  if (l.status === "revoked") return "revoked";
  const left = Date.parse(l.valid_until) - now;
  if (l.status === "expired" || left <= 0) return "expired";
  return left <= RENEW_WITHIN_DAYS * DAY ? "soon" : "ok";
}

export const REVOKE_REASON: Record<PersonalLoginRevokeReason, string> = {
  access_lost: "access to the database was lost",
  suspended: "the member was suspended",
  revoked_now: "revoked by a tenant admin",
  identity_offboarded: "the sign-in account was switched off",
  member_removed: "the member was removed",
};

const RELATIVE = new Intl.RelativeTimeFormat("en", { numeric: "always" });
/** "in 71 days", "2 days ago": whole days from a day on (a login's life is counted in days), else the shorter units. */
export function daysFrom(iso: string, now: number = Date.now()): string {
  const diff = Date.parse(iso) - now;
  const days = Math.round(diff / DAY);
  if (days !== 0) return RELATIVE.format(days, "day");
  const hours = Math.round(diff / 3_600_000);
  if (hours !== 0) return RELATIVE.format(hours, "hour");
  return RELATIVE.format(Math.round(diff / 60_000), "minute");
}

const DATE = new Intl.DateTimeFormat("en-GB", { day: "numeric", month: "short", year: "numeric", timeZone: "UTC" });
/** "2 Jan 2027" (UTC, so everyone reads the same day). */
export function dateText(iso: string): string {
  return DATE.format(new Date(iso));
}

/** The first word of a name: "Sofia Marchetti" is "Sofia". */
export function firstName(name: string): string {
  return name.trim().split(/\s+/)[0] ?? name;
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}
