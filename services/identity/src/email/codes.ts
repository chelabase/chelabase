// Email code storage: one live six-digit code per (user, kind) in the pool's own schema.
//
// Why not Better Auth's `email-otp` storage (1.7.15, dist/plugins/email-otp): its codes live
// in `verification` under the identifier `<type>-otp-<email>`, so they are bound to an email,
// not a user id; there is one `expiresIn` for every type; invite and reset would share the
// one `forget-password` identifier, so an anonymous reset request replaces a pending invite;
// and a guess is counted by a read, a delete and a re-insert (`atomicVerifyOTP`) rather than in
// the statement that consumes the code. So the plugin is not loaded, and this table, created
// by Better Auth's own migrations from a schema-only plugin, holds the codes instead.
//
// Invariants:
// - a code is stored only as an HMAC-SHA256 under the service secret, over the pool schema,
//   user id, lowercased email, kind and code, so a row is worthless in another pool, for
//   another user or kind, and a database dump can't be brute-forced without the secret;
// - a code proves control of the address it was sent to: it is checked against the user's
//   current email, and every address change (`users.set_email`, the `email_change` swap) goes
//   through `CodeStore.changeEmail`, which revokes every outstanding code for that user in the
//   same statement as the update; so a code never outlives the address it was bound to, even
//   when the address later changes back;
// - an `email_change` code is sent to the requested new address, and is bound to both the
//   current address and that new one (`CodeBinding`), so it proves the new mailbox for that
//   user only while the current address is unchanged, and only for that new address; no
//   pending address is stored;
// - a guess is counted and the code consumed in one UPDATE: a row whose attempts reached
//   MAX_ATTEMPTS, or that is expired or consumed, never matches again;
// - each (user, kind) row also holds a failure budget (`failures` within the window that
//   started at `failuresSince`): FAILURE_BUDGET wrong guesses per FAILURE_WINDOW_MS, across
//   every code of that kind. The budget check, the count and the consume are the same UPDATE,
//   so concurrent guesses serialize on the row and none matches once the budget is spent: the
//   budget is never exceeded and a spent budget refuses before the code is compared. Only a
//   right code clears it. NULLs (rows from before the columns existed) read as unspent;
// - the row outlives its codes: `issue` replaces the code and its attempts but never touches
//   the budget, and revoking codes on an email change marks them consumed rather than
//   deleting the row; so a new code never refills the budget;
// - a live `verify`, `reset` or `email_change` code younger than REPLACE_AFTER_MS is not
//   replaced: the upsert itself carries the condition, so of two concurrent issues at most one
//   replaces. `invite` is exempt (tenant-admin resends, budgeted by the orchestrator).
import { createHmac, randomInt, randomUUID } from "node:crypto";
import type pg from "pg";

export const CODE_KINDS = ["verify", "reset", "invite", "email_change"] as const;
export type CodeKind = (typeof CODE_KINDS)[number];

const MINUTE_MS = 60_000;
export const CODE_TTL_MS: Record<CodeKind, number> = {
  verify: 15 * MINUTE_MS,
  reset: 15 * MINUTE_MS,
  invite: 7 * 24 * 60 * MINUTE_MS,
  email_change: 15 * MINUTE_MS,
};
/** Human wording of each TTL for the templates. */
export const CODE_TTL_TEXT: Record<CodeKind, string> = {
  verify: "15 minutes",
  reset: "15 minutes",
  invite: "7 days",
  email_change: "15 minutes",
};
/** Wrong guesses a code survives; the next guess, right or wrong, is refused. */
export const MAX_ATTEMPTS = 5;
/** Wrong guesses a (user, kind) may make per FAILURE_WINDOW_MS, across codes. */
export const FAILURE_BUDGET = 10;
export const FAILURE_WINDOW_MS = 24 * 60 * MINUTE_MS;
/** A live code younger than this is not replaced (every kind but `invite`). */
export const REPLACE_AFTER_MS = MINUTE_MS;
/** Kinds whose live codes may be replaced at once. */
const REPLACE_ANYTIME: readonly CodeKind[] = ["invite"];

/** One guess's outcome: consumed, refused (wrong, or no live code), or refused unchecked. */
export type ConsumeResult = "ok" | "wrong" | "budget_spent";

const TABLE = "email_code";

/**
 * The address(es) a code is bound to: the user's current email for `verify`, `reset` and
 * `invite`; the current email and the requested new one for `email_change`.
 */
export type CodeBinding = string | { current: string; next: string };

/**
 * The bound address(es) as one hash field. The current address is length-prefixed, so no pair
 * of addresses encodes like another pair; the kind follows in its own field, so an
 * `email_change` binding never matches another kind's.
 */
function bindingText(kind: CodeKind, binding: CodeBinding): string {
  if (kind === "email_change") {
    if (typeof binding !== "object") throw new Error("an email_change code is bound to the current and the new address");
    const current = binding.current.toLowerCase();
    return `${current.length}:${current}\0${binding.next.toLowerCase()}`;
  }
  if (typeof binding !== "string") throw new Error(`a ${kind} code is bound to the current address only`);
  return binding.toLowerCase();
}

/**
 * The table as a Better Auth plugin schema, so `getMigrations` creates it in each pool's schema
 * (in search_path mode, like the core tables). Rows go with their user (`ON DELETE CASCADE`).
 */
export function codesSchemaPlugin() {
  return {
    id: "chelabase-email-codes",
    schema: {
      emailCode: {
        modelName: TABLE,
        fields: {
          userId: { type: "string", required: true, references: { model: "user", field: "id", onDelete: "cascade" } },
          kind: { type: "string", required: true },
          codeHash: { type: "string", required: true },
          attempts: { type: "number", required: true },
          expiresAt: { type: "date", required: true },
          consumedAt: { type: "date", required: false },
          createdAt: { type: "date", required: true },
          // Nullable with a constant default, so adding them to a pool with rows rewrites nothing
          // and needs no backfill; NULL reads as 0 / no window.
          failures: { type: "number", required: false, defaultValue: 0 },
          failuresSince: { type: "date", required: false },
        },
        indexes: [{ fields: ["userId", "kind"], unique: true, name: "email_code_user_kind_key" }],
      },
    },
  } as const;
}

/** Whether the row's failure window is still open; `start` is the placeholder for now - window. */
function windowOpen(start: string): string {
  return `("failuresSince" IS NOT NULL AND "failuresSince" > ${start}::timestamptz)`;
}

/** Whether the row's failure budget is spent; NULL failures read as 0. */
function budgetSpent(start: string, budget: string): string {
  return `(${windowOpen(start)} AND COALESCE(failures, 0) >= ${budget}::int)`;
}

export class CodeStore {
  private readonly db: pg.Pool;
  private readonly schema: string;
  private readonly secret: string;
  private readonly table: string;
  private readonly users: string;

  /** `schema` is a validated pool schema name (pool-name.ts SCHEMA_PATTERN). */
  constructor(db: pg.Pool, schema: string, secret: string) {
    if (!/^[a-z0-9_]{1,63}$/.test(schema)) throw new Error("refusing an unvalidated schema name");
    this.db = db;
    this.schema = schema;
    this.secret = secret;
    this.table = `"${schema}"."${TABLE}"`;
    this.users = `"${schema}"."user"`;
  }

  private hash(userId: string, binding: CodeBinding, kind: CodeKind, code: string): string {
    return createHmac("sha256", this.secret)
      .update(`chelabase-identity email-code v2\0${this.schema}\0${userId}\0${bindingText(kind, binding)}\0${kind}\0${code}`)
      .digest("base64url");
  }

  /**
   * A fresh code for (user, kind), replacing any earlier one, bound to `binding`: the user's
   * current address, plus the new one for `email_change`. The caller checked the user. Null
   * when a live code younger than REPLACE_AFTER_MS stands (never for `invite`): that code is
   * kept. The failure budget carries over to the new code.
   */
  async issue(
    userId: string,
    binding: CodeBinding,
    kind: CodeKind,
    now = new Date(),
  ): Promise<{ code: string; expiresAt: Date } | null> {
    const code = String(randomInt(0, 1_000_000)).padStart(6, "0");
    const expiresAt = new Date(now.getTime() + CODE_TTL_MS[kind]);
    const r = await this.db.query(
      `INSERT INTO ${this.table} AS c (id, "userId", kind, "codeHash", attempts, "expiresAt", "consumedAt", "createdAt")
       VALUES ($1, $2, $3, $4, 0, $5, NULL, $6)
       ON CONFLICT ("userId", kind) DO UPDATE SET
         "codeHash" = EXCLUDED."codeHash", attempts = 0, "expiresAt" = EXCLUDED."expiresAt",
         "consumedAt" = NULL, "createdAt" = EXCLUDED."createdAt"
       WHERE $7::boolean OR NOT (
         c."consumedAt" IS NULL AND c.attempts < $8 AND c."expiresAt" > $6::timestamptz AND c."createdAt" > $9::timestamptz
       )
       RETURNING 1`,
      [
        randomUUID(),
        userId,
        kind,
        this.hash(userId, binding, kind, code),
        expiresAt,
        now,
        REPLACE_ANYTIME.includes(kind),
        MAX_ATTEMPTS,
        new Date(now.getTime() - REPLACE_AFTER_MS),
      ],
    );
    return r.rowCount === 1 ? { code, expiresAt } : null;
  }

  /**
   * Withdraws a just-issued code that never reached its mailbox (the send was refused): marks
   * it consumed, so it no longer holds the replacement window and a retry issues a new one.
   * Matched by its hash, so a code another request issued meanwhile is left alone. The
   * failure budget stays.
   */
  async withdraw(userId: string, binding: CodeBinding, kind: CodeKind, code: string, now = new Date()): Promise<void> {
    await this.db.query(
      `UPDATE ${this.table} SET "consumedAt" = $4
        WHERE "userId" = $1 AND kind = $2 AND "codeHash" = $3 AND "consumedAt" IS NULL`,
      [userId, kind, this.hash(userId, binding, kind, code), now],
    );
  }

  /**
   * Sets the user's email and verified flag and, when the address really changes (compared
   * case-insensitively), revokes every outstanding code of every kind for that user (marks it
   * consumed; the row and its failure budget stay). One statement, so the update and the
   * revocation commit together. With `expected`, only while
   * the user's current email is exactly that. Returns the previous address, or null when no
   * row matched (no such user, or the email is no longer `expected`). A unique violation on
   * the email (23505) propagates. `email` is already lowercased and checked by the caller.
   */
  async changeEmail(
    userId: string,
    email: string,
    verified: boolean,
    expected?: string,
    now = new Date(),
  ): Promise<{ previous: string } | null> {
    const r = await this.db.query<{ previous: string }>(
      `WITH prev AS (
         SELECT id, email FROM ${this.users}
          WHERE id = $1 AND ($4::text IS NULL OR email = $4::text)
          FOR UPDATE
       ), moved AS (
         UPDATE ${this.users} u SET email = $2, "emailVerified" = $3, "updatedAt" = $5
           FROM prev WHERE u.id = prev.id
         RETURNING prev.email AS previous
       ), revoked AS (
         UPDATE ${this.table} SET "consumedAt" = $5
          WHERE "userId" = $1 AND "consumedAt" IS NULL
            AND EXISTS (SELECT 1 FROM moved WHERE lower(moved.previous) <> lower($2))
       )
       SELECT previous FROM moved`,
      [userId, email, verified, expected ?? null, now],
    );
    return r.rows[0] ?? null;
  }

  /**
   * One guess. Checks the failure budget, counts the attempt and the failure and, when the
   * code matches, consumes it and clears the budget, all in one statement: concurrent guesses
   * serialize on the row, and each re-checks the conditions (budget included) after the
   * previous one committed. A spent budget matches no row, so the code is never compared.
   * `binding` holds the user's current address (and, for `email_change`, the new address the
   * caller names): a code bound to anything else never matches (it still counts).
   */
  async consume(
    userId: string,
    binding: CodeBinding,
    kind: CodeKind,
    code: string,
    now = new Date(),
  ): Promise<ConsumeResult> {
    const windowStart = new Date(now.getTime() - FAILURE_WINDOW_MS);
    const r = await this.db.query<{ ok: boolean }>(
      `UPDATE ${this.table} SET
         attempts = attempts + 1,
         "consumedAt" = CASE WHEN "codeHash" = $3 THEN $4::timestamptz END,
         failures = CASE WHEN "codeHash" = $3 THEN 0 WHEN ${windowOpen("$6")} THEN COALESCE(failures, 0) + 1 ELSE 1 END,
         "failuresSince" = CASE WHEN "codeHash" = $3 THEN NULL WHEN ${windowOpen("$6")} THEN "failuresSince" ELSE $4::timestamptz END
       WHERE "userId" = $1 AND kind = $2 AND "consumedAt" IS NULL AND attempts < $5 AND "expiresAt" > $4::timestamptz
         AND NOT ${budgetSpent("$6", "$7")}
       RETURNING "consumedAt" IS NOT NULL AS ok`,
      [userId, kind, this.hash(userId, binding, kind, code), now, MAX_ATTEMPTS, windowStart, FAILURE_BUDGET],
    );
    if (r.rows[0]) return r.rows[0].ok ? "ok" : "wrong";
    // No live code, or a spent budget: one read tells them apart.
    const left = await this.db.query<{ spent: boolean }>(
      `SELECT ${budgetSpent("$3", "$4")} AS spent FROM ${this.table} WHERE "userId" = $1 AND kind = $2`,
      [userId, kind, windowStart, FAILURE_BUDGET],
    );
    return left.rows[0]?.spent === true ? "budget_spent" : "wrong";
  }
}
