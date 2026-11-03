import { readFileSync } from "node:fs";
import type pg from "pg";
import { expect, it } from "vitest";
import { authOptions } from "../src/better-auth.ts";

const DAY = 24 * 60 * 60;
const THIRTY_DAYS = 30 * DAY;

// #121: `/auth/v1/token` reads sessions without sliding them, so this is how long one lives.
it("session_lives_thirty_days", () => {
  const options = authOptions({} as pg.Pool, "s".repeat(32), "identity_members");
  expect(options.session.expiresIn).toBe(THIRTY_DAYS);
});

/** The orchestrator's `APP_SESSION_CAP` (an app session's 90-day cap), read from its source. */
function appSessionCapSecs(): number {
  const path = new URL("../../orchestrator/src/internal/config.rs", import.meta.url);
  const source = readFileSync(path, "utf8");
  const m = /pub const APP_SESSION_CAP: Duration = Duration::from_secs\(([0-9 *]+)\);/.exec(source);
  if (!m) throw new Error("APP_SESSION_CAP not found in the orchestrator's config.rs");
  return m[1]!.split("*").reduce((n, f) => n * Number(f.trim()), 1);
}

// #123: an app session's refresh checks its identity session by id, which never slides it. So an
// app pool's identity session lives as long as the app session's cap: refresh keeps its 90 days.
it("app_pool_sessions_live_as_long_as_the_app_session_cap", () => {
  const cap = appSessionCapSecs();
  expect(cap).toBe(90 * DAY);
  const options = authOptions({} as pg.Pool, "s".repeat(32), "identity_app_0123456789abcdef0123456789abcdef");
  expect(options.session.expiresIn).toBe(cap);
});
