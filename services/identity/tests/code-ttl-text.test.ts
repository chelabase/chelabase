// An emailed code's stated lifetime must be the one enforced. No database needed.
import { readFileSync } from "node:fs";
import { expect, it } from "vitest";
import { CODE_KINDS, CODE_TTL_MS, CODE_TTL_TEXT } from "../src/email/codes.ts";

const UNIT_MS: Record<string, number> = { minute: 60_000, hour: 3_600_000, day: 86_400_000 };

/** "15 minutes" -> 900000, "72 hours" -> 259200000. Anything else throws. */
function parseTtlText(text: string): number {
  const m = /^(\d+) (minute|hour|day)s?$/.exec(text);
  if (!m) throw new Error(`unparseable TTL text: ${text}`);
  return Number(m[1]) * UNIT_MS[m[2]!]!;
}

for (const kind of CODE_KINDS) {
  it(`ttl_text_matches_ttl_${kind}`, () => {
    expect(parseTtlText(CODE_TTL_TEXT[kind])).toBe(CODE_TTL_MS[kind]);
  });
}

/** The orchestrator's `INVITE_LIFETIME_HOURS`, read from its source. */
function inviteLifetimeHours(): number {
  const path = new URL("../../orchestrator/src/members.rs", import.meta.url);
  const m = /const INVITE_LIFETIME_HOURS: i64 = (\d+);/.exec(readFileSync(path, "utf8"));
  if (!m) throw new Error("INVITE_LIFETIME_HOURS not found in the orchestrator's members.rs");
  return Number(m[1]);
}

it("invite_code_lives_as_long_as_the_invite", () => {
  const hours = inviteLifetimeHours();
  expect(hours).toBe(72);
  expect(CODE_TTL_MS.invite).toBe(hours * UNIT_MS.hour!);
  expect(CODE_TTL_TEXT.invite).toBe("72 hours");
});
