import type { AccessEvent, Member, PlatformRole } from "@chelabase/management-client";

/** A member's name: their display name, else their email's local part. */
export function memberName(m: Pick<Member, "display_name" | "email">): string {
  if (m.display_name) return m.display_name;
  if (m.email) return m.email.split("@")[0];
  return "Unnamed member";
}

export const ROLE_LABEL: Record<PlatformRole, string> = { viewer: "Viewer", developer: "Developer", admin: "Admin" };

/** "2026-10-11 12:00 UTC": the absolute time shown on hover and focus. */
export function utc(iso: string): string {
  return `${new Date(iso).toISOString().slice(0, 16).replace("T", " ")} UTC`;
}

const UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 365 * 86_400_000],
  ["month", 30 * 86_400_000],
  ["day", 86_400_000],
  ["hour", 3_600_000],
  ["minute", 60_000],
];
const RELATIVE = new Intl.RelativeTimeFormat("en", { numeric: "always" });

/** "3 days ago", "in 5 minutes", "just now": the largest whole unit. */
export function relativeTime(iso: string, now: Date = new Date()): string {
  const diff = new Date(iso).getTime() - now.getTime();
  for (const [unit, ms] of UNITS) {
    const n = Math.round(diff / ms);
    if (n !== 0) return RELATIVE.format(n, unit);
  }
  return "just now";
}

export interface StatusText {
  text: string;
  /** plain: chalk; dim: chalk-dim; hazard: coral (the invite link no longer works). */
  tone: "plain" | "dim" | "hazard";
  /** The absolute time behind a relative one. */
  title?: string;
}

/** A member's status, written as words. */
export function memberStatus(m: Member, now: Date = new Date()): StatusText {
  if (m.status === "pending") return { text: "Waiting for approval", tone: "dim" };
  if (m.status === "invited") {
    if (!m.invite) return { text: "Invited", tone: "plain" };
    const expires = m.invite.expires_at;
    if (new Date(expires).getTime() <= now.getTime()) return { text: "Invite expired", tone: "hazard", title: utc(expires) };
    return { text: `Invited · expires ${relativeTime(expires, now)}`, tone: "plain", title: utc(expires) };
  }
  if (m.status === "suspended") return { text: "Suspended", tone: "hazard" };
  if (!m.has_access && !m.tenant_admin) return { text: "No access yet", tone: "dim" };
  return { text: "Active", tone: "plain" };
}

export interface SentenceContext {
  /** Member id → display name, from the members list. */
  names: Map<string, string>;
  /** A database's name, or null when it's gone. */
  databaseName: (id: string) => string | null;
  /** A project's name, or null when it's gone (for grants inside an event). */
  projectName?: (id: string) => string | null;
}

interface GrantLike {
  project_id: string;
  platform_role: PlatformRole;
  pg_roles: string[];
}

const grantsOf = (v: unknown): GrantLike[] => {
  const g = field(v, "grants");
  if (!Array.isArray(g)) return [];
  return g.filter(
    (x): x is GrantLike =>
      x !== null && typeof x === "object" && typeof (x as GrantLike).project_id === "string" && (x as GrantLike).platform_role in ROLE_LABEL,
  );
};
const projectOf = (id: string, ctx: SentenceContext) => ctx.projectName?.(id) ?? "a deleted project";

const field = (v: unknown, key: string): unknown =>
  v !== null && typeof v === "object" ? (v as Record<string, unknown>)[key] : undefined;
const roleOf = (v: unknown): string | null => {
  const r = field(v, "platform_role");
  return typeof r === "string" && r in ROLE_LABEL ? ROLE_LABEL[r as PlatformRole] : null;
};
const rolesOf = (v: unknown): string[] => {
  const r = field(v, "pg_roles");
  return Array.isArray(r) ? r.filter((x): x is string => typeof x === "string") : [];
};

/** A login's or role's name from an event's `after` (else `before`), as " name" (or empty). */
const named = (e: AccessEvent, keys: string[], prefix: string): string => {
  for (const side of [e.after, e.before]) {
    for (const k of keys) {
      const v = field(side, k);
      if (typeof v === "string" && v !== "") return `${prefix} ${v}`;
    }
  }
  return "";
};
const loginName = (e: AccessEvent) => named(e, ["login", "role", "name"], "");
const roleName = (e: AccessEvent, prefix = "") => named(e, ["role", "name"], prefix);

/** One access event as a sentence (the Activity list and the project's access log). */
export function eventSentence(e: AccessEvent, ctx: SentenceContext): string {
  const person = (id: string | null, email: string | null, system: string) => {
    if (id === null && email === null) return system;
    return (id !== null && ctx.names.get(id)) || email || "a former member";
  };
  const actor = person(e.actor_member_id, e.actor_email, "Chelabase");
  const target = person(e.target_member_id, e.target_email, "someone");
  const project = e.project_name ?? "a deleted project";
  const database = e.database_id ? (ctx.databaseName(e.database_id) ?? "a deleted database") : "a database";
  switch (e.action) {
    case "member_joined":
      // A sign-up joins as pending; an org user admitted at once signed in.
      return field(e.after, "status") === "pending" ? `${target} signed up` : `${target} signed in for the first time`;
    case "member_added":
      return `${actor} added ${target} (already had an account)`;
    case "admin_claimed":
      return `${target} became the first tenant admin`;
    case "admin_set":
      return `${actor} made ${target} a tenant admin`;
    case "admin_unset":
      return `${actor} removed ${target} as a tenant admin`;
    case "member_removed":
      return `${actor} removed ${target}`;
    case "grant_put": {
      const after = roleOf(e.after) ?? "access";
      const before = roleOf(e.before);
      if (before && before === after) {
        const roles = (v: unknown) => rolesOf(v).join(", ") || "none";
        return `${actor} changed ${target}'s Postgres roles on ${project}: ${roles(e.before)} → ${roles(e.after)}`;
      }
      return before
        ? `${actor} changed ${target}'s access on ${project}: ${before} → ${after}`
        : `${actor} gave ${target} ${after} on ${project}`;
    }
    case "grant_deleted":
      return `${actor} took ${target}'s access to ${project} away`;
    case "override_put": {
      const parts = [roleOf(e.after), rolesOf(e.after).join(", ") || null].filter(Boolean);
      return `${actor} narrowed ${target} on ${database} to ${parts.join(", ") || "less"}`;
    }
    case "override_deleted":
      return `${actor} stopped narrowing ${target} on ${database}`;
    case "invite_created": {
      const email = field(e.after, "email");
      return `${actor} invited ${typeof email === "string" ? email : target}`;
    }
    case "invite_resent":
      return `${actor} sent the invite to ${target} again`;
    case "invite_revoked":
      return `${actor} revoked the invite to ${target}`;
    case "invite_accepted":
      return `${target} accepted the invite`;
    case "open_signup_set":
      return `${actor} turned open sign-up ${field(e.after, "open_signup") === true ? "on" : "off"}`;
    case "tenant_renamed": {
      const name = field(e.after, "display_name");
      return typeof name === "string" && name !== ""
        ? `${actor} renamed the tenant to ${name}`
        : `${actor} cleared the tenant's name`;
    }
    case "member_approved": {
      const grants = grantsOf(e.after).map((g) => `${ROLE_LABEL[g.platform_role]} on ${projectOf(g.project_id, ctx)}`);
      const given = grants.length === 0 ? "no project access" : grants.join(" and ");
      return `${actor} approved ${target}'s sign-up with ${given}`;
    }
    case "member_declined":
      return `${actor} declined ${target}'s sign-up`;
    case "member_suspended":
      return `${actor} suspended ${target}`;
    case "member_reinstated":
      return `${actor} reinstated ${target}`;
    case "personal_logins_revoked":
      return `${actor} revoked all of ${target}'s personal Postgres logins`;
    case "personal_login_created":
      return `${actor} created a personal Postgres login${loginName(e)} for ${target}${e.database_id ? ` on ${database}` : ""}`;
    case "personal_login_reset":
      return `${actor} reset the password of ${target}'s personal Postgres login${loginName(e)}`;
    case "personal_login_deleted":
      return `${actor} deleted ${target}'s personal Postgres login${loginName(e)}`;
    case "role_created":
      return `${actor} created the Postgres role${roleName(e)}${e.database_id ? ` on ${database}` : ""}`;
    case "role_updated":
      return `${actor} changed the Postgres role${roleName(e)}${e.database_id ? ` on ${database}` : ""}`;
    case "role_deleted":
      return `${actor} deleted the Postgres role${roleName(e)}${e.database_id ? ` on ${database}` : ""}`;
    case "privileges_changed":
      return `${actor} changed privileges${roleName(e, " of")}${e.database_id ? ` on ${database}` : ""}`;
    default:
      return `${actor} changed access (${e.action})`;
  }
}

/** One labelled field of an event's `before` or `after`, for its details. */
export interface EventField {
  label: string;
  value: string;
}

const STATUS_WORDS: Record<string, string> = { active: "Active", invited: "Invited", pending: "Waiting for approval", removed: "Removed", suspended: "Suspended" };

/**
 * An event's `before` or `after` as labelled, readable fields (projects and databases by name,
 * roles as words, times in UTC); a field Studio doesn't know yet keeps its key and compact JSON.
 */
export function eventFields(value: unknown, ctx: SentenceContext, action?: string): EventField[] {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return [];
  const out: EventField[] = [];
  for (const [key, v] of Object.entries(value as Record<string, unknown>)) {
    if (key === "project_id" && typeof v === "string") out.push({ label: "Project", value: projectOf(v, ctx) });
    else if (key === "status" && typeof v === "string") out.push({ label: "Status", value: STATUS_WORDS[v] ?? v });
    else if (key === "grants") {
      const grants = grantsOf(value).map(
        (g) => `${ROLE_LABEL[g.platform_role]} on ${projectOf(g.project_id, ctx)}${g.pg_roles.length > 0 ? ` (${g.pg_roles.join(", ")})` : ""}`,
      );
      out.push({ label: "Access", value: grants.length > 0 ? grants.join("; ") : "No project access" });
    } else if (key === "expires_at" && typeof v === "string") out.push({ label: "Expires", value: utc(v) });
    else if (key === "email") out.push({ label: "Email", value: typeof v === "string" ? v : "Unknown" });
    else if (key === "tenant_admin" && typeof v === "boolean") out.push({ label: "Tenant admin", value: v ? "Yes" : "No" });
    else if (key === "open_signup" && typeof v === "boolean") out.push({ label: "Open sign-up", value: v ? "On" : "Off" });
    else if (key === "display_name") out.push({ label: "Tenant name", value: typeof v === "string" && v !== "" ? v : "None (shows the slug)" });
    else if (key === "database_id" && typeof v === "string") out.push({ label: "Database", value: ctx.databaseName(v) ?? "a deleted database" });
    else if (key === "platform_role" && typeof v === "string" && v in ROLE_LABEL) out.push({ label: "Platform role", value: ROLE_LABEL[v as PlatformRole] });
    else if (key === "platform_role" && v === null) out.push({ label: "Platform role", value: "Same as the project" });
    else if (key === "pg_roles") {
      const roles = rolesOf(value);
      // R17: an Admin grant holds every role, so it names none.
      const every = action === "grant_put" && field(value, "platform_role") === "admin" && roles.length === 0;
      out.push({ label: "Postgres roles", value: every ? "All roles" : v === null ? "All of the project's roles" : roles.length > 0 ? roles.join(", ") : "None" });
    } else out.push({ label: key, value: typeof v === "string" ? v : JSON.stringify(v) });
  }
  return out;
}

/** The first letter of the first and the last word, upper-cased; whole code points, so a surrogate pair is never split. */
export function initials(name: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  const letters = words.length > 1 ? [words[0], words[words.length - 1]] : words;
  return letters.map((w) => Array.from(w)[0] ?? "").join("").toUpperCase();
}
