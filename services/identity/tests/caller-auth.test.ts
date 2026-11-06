// Caller-token configuration checks. No database needed.
import { randomBytes } from "node:crypto";
import { describe, expect, it } from "vitest";
import { CallerAuth } from "../src/caller-auth.ts";
import type { Caller } from "../src/config.ts";
import { startServer } from "../src/server.ts";

describe("caller tokens", () => {
  // N3: two callers sharing a token would silently resolve to one of them.
  it("refuses_duplicate_tokens", async () => {
    const shared = randomBytes(32).toString("hex");
    const tokens = new Map<Caller, string>([
      ["auth", shared],
      ["orchestrator", randomBytes(32).toString("hex")],
      ["worker", shared],
    ]);
    expect(() => new CallerAuth(tokens)).toThrow(/auth and worker share a token/);
    await expect(
      startServer({
        databaseUrl: "postgres://nobody@127.0.0.1:1/none",
        secret: randomBytes(32).toString("hex"),
        callerTokens: tokens,
        poolCacheSize: 1,
        host: "127.0.0.1",
        port: 0,
      }),
    ).rejects.toThrow(/share a token/);
  });
});
