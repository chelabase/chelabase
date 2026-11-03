import type pg from "pg";
import { expect, it } from "vitest";
import { authOptions } from "../src/better-auth.ts";

const THIRTY_DAYS = 30 * 24 * 60 * 60;

// #121: `/auth/v1/token` reads sessions without sliding them, so this is how long one lives.
it("session_lives_thirty_days", () => {
  const options = authOptions({} as pg.Pool, "s".repeat(32), "identity_members");
  expect(options.session.expiresIn).toBe(THIRTY_DAYS);
});
