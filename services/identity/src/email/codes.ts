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
//   user id, kind and code, so a row is worthless in another pool, for another user or kind,
//   and a database dump can't be brute-forced without the secret;
// - a guess is counted and the code consumed in one UPDATE: a row whose attempts reached
//   MAX_ATTEMPTS, or that is expired or consumed, never matches again;
// - a new code for the same (user, kind) replaces the row, so the old code dies with its
//   counter; every code gets at most MAX_ATTEMPTS guesses.
import { createHmac, randomInt, randomUUID } from "node:crypto";
import type pg from "pg";

export const CODE_KINDS = ["verify", "reset", "invite"] as const;
export type CodeKind = (typeof CODE_KINDS)[number];

const MINUTE_MS = 60_000;
export const CODE_TTL_MS: Record<CodeKind, number> = {
  verify: 15 * MINUTE_MS,
  reset: 15 * MINUTE_MS,
  invite: 7 * 24 * 60 * MINUTE_MS,
};
/** Human wording of each TTL for the templates. */
export const CODE_TTL_TEXT: Record<CodeKind, string> = { verify: "15 minutes", reset: "15 minutes", invite: "7 days" };
/** Wrong guesses a code survives; the next guess, right or wrong, is refused. */
export const MAX_ATTEMPTS = 5;

const TABLE = "email_code";

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
        },
        indexes: [{ fields: ["userId", "kind"], unique: true, name: "email_code_user_kind_key" }],
      },
    },
  } as const;
}

export class CodeStore {
  private readonly db: pg.Pool;
  private readonly schema: string;
  private readonly secret: string;
  private readonly table: string;

  /** `schema` is a validated pool schema name (pool-name.ts SCHEMA_PATTERN). */
  constructor(db: pg.Pool, schema: string, secret: string) {
    if (!/^[a-z0-9_]{1,63}$/.test(schema)) throw new Error("refusing an unvalidated schema name");
    this.db = db;
    this.schema = schema;
    this.secret = secret;
    this.table = `"${schema}"."${TABLE}"`;
  }

  private hash(userId: string, kind: CodeKind, code: string): string {
    return createHmac("sha256", this.secret)
      .update(`chelabase-identity email-code v1\0${this.schema}\0${userId}\0${kind}\0${code}`)
      .digest("base64url");
  }

  /** A fresh code for (user, kind), replacing any earlier one. The caller checked the user. */
  async issue(userId: string, kind: CodeKind, now = new Date()): Promise<{ code: string; expiresAt: Date }> {
    const code = String(randomInt(0, 1_000_000)).padStart(6, "0");
    const expiresAt = new Date(now.getTime() + CODE_TTL_MS[kind]);
    await this.db.query(
      `INSERT INTO ${this.table} (id, "userId", kind, "codeHash", attempts, "expiresAt", "consumedAt", "createdAt")
       VALUES ($1, $2, $3, $4, 0, $5, NULL, $6)
       ON CONFLICT ("userId", kind) DO UPDATE SET
         "codeHash" = EXCLUDED."codeHash", attempts = 0, "expiresAt" = EXCLUDED."expiresAt",
         "consumedAt" = NULL, "createdAt" = EXCLUDED."createdAt"`,
      [randomUUID(), userId, kind, this.hash(userId, kind, code), expiresAt, now],
    );
    return { code, expiresAt };
  }

  /**
   * One guess. Counts the attempt and, when the code matches, consumes it, in one statement:
   * concurrent guesses serialize on the row, and each re-checks the conditions after the
   * previous one committed.
   */
  async consume(userId: string, kind: CodeKind, code: string, now = new Date()): Promise<boolean> {
    const r = await this.db.query<{ ok: boolean }>(
      `UPDATE ${this.table} SET
         attempts = attempts + 1,
         "consumedAt" = CASE WHEN "codeHash" = $3 THEN $4::timestamptz END
       WHERE "userId" = $1 AND kind = $2 AND "consumedAt" IS NULL AND attempts < $5 AND "expiresAt" > $4::timestamptz
       RETURNING "consumedAt" IS NOT NULL AS ok`,
      [userId, kind, this.hash(userId, kind, code), now, MAX_ATTEMPTS],
    );
    return r.rows[0]?.ok === true;
  }
}
