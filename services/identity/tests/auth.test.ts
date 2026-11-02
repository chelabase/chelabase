import { generateKeyPairSync, sign } from "node:crypto";
import { afterAll, beforeAll, expect, it } from "vitest";
import { call, describeDb, newAppPool, schemaExists, startHarness, type Harness } from "./harness.ts";

/** A token shaped like the gateway's internal identity token (EdDSA JWT, aud = identity). */
function gatewayToken(expOffsetSecs: number): string {
  const { privateKey } = generateKeyPairSync("ed25519");
  const b64 = (v: unknown) => Buffer.from(JSON.stringify(v)).toString("base64url");
  const now = Math.floor(Date.now() / 1000);
  const head = b64({ alg: "EdDSA", typ: "JWT" });
  const claims = b64({
    iss: "chelabase-gateway",
    aud: "identity",
    iat: now - 120,
    exp: now + expOffsetSecs,
    sub: "user-1",
    org_id: "org-1",
    auth: "management_jwt",
    rid: "r1",
  });
  const sig = sign(null, Buffer.from(`${head}.${claims}`), privateKey).toString("base64url");
  return `${head}.${claims}.${sig}`;
}

describeDb("caller auth", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness();
  });
  afterAll(async () => {
    await h?.close();
  });

  const routes = (pool: string): [string, string][] => [
    ["POST", `/internal/v1/pools/${pool}`],
    ["DELETE", `/internal/v1/pools/${pool}`],
    ["POST", `/internal/v1/pools/${pool}/users`],
    ["POST", `/internal/v1/pools/${pool}/sessions`],
    ["POST", `/internal/v1/pools/${pool}/sessions/lookup`],
    ["GET", "/internal/v1/no-such-route"],
  ];

  async function expectAll401(authorization: string | undefined, headers: Record<string, string> = {}) {
    const p = newAppPool();
    for (const [method, path] of routes(p.pool)) {
      const r = await call(h, method, path, {
        as: null,
        ...(authorization === undefined ? {} : { authorization }),
        headers,
        body: { email: "a@x.io", password: "password-x-1", token: "t" },
      });
      expect(r.status, `${method} ${path}`).toBe(401);
    }
    expect(await schemaExists(h.db, p.schema)).toBe(false);
  }

  // AC 2: no token at all.
  it("no_token_401", async () => {
    await expectAll401(undefined);
    await expectAll401("");
    await expectAll401("Bearer ");
    await expectAll401(`Basic ${h.tokens.auth}`);
  });

  // AC 2: a token that belongs to no listed caller (including a near miss of a real one).
  it("unknown_caller_401", async () => {
    await expectAll401("Bearer not-a-caller-token-but-long-enough-0123456789");
    await expectAll401(`Bearer ${h.tokens.auth.slice(0, -1)}`);
    await expectAll401(`Bearer ${h.tokens.auth}x`);
    await expectAll401(`Bearer ${h.tokens.auth.toUpperCase()}`);
  });

  // AC 2: tokens carry no expiry of their own (they are static files); an expired
  // gateway-style token is still just an unknown token.
  it("expired_token_401", async () => {
    await expectAll401(`Bearer ${gatewayToken(-60)}`);
  });

  // AC 8: the gateway-minted internal identity token is never a caller credential, in either
  // header, even while it is unexpired.
  it("gateway_token_refused", async () => {
    const token = gatewayToken(60);
    await expectAll401(`Bearer ${token}`);
    await expectAll401(undefined, { "x-chelabase-identity": token });
  });

  // AC 8: the worker manages pools and reads users, nothing else.
  it("worker_route_allow_list", async () => {
    const p = newAppPool();
    const created = await call(h, "POST", `/internal/v1/pools/${p.pool}`, { as: "worker" });
    expect(created.status).toBe(201);
    for (const [method, path] of [
      ["POST", `/internal/v1/pools/${p.pool}/users`],
      ["POST", `/internal/v1/pools/${p.pool}/sessions`],
      ["POST", `/internal/v1/pools/${p.pool}/sessions/lookup`],
    ] as const) {
      const r = await call(h, method, path, {
        as: "worker",
        body: { email: "a@x.io", password: "password-x-1", token: "t" },
      });
      expect(r.status, `${method} ${path}`).toBe(403);
    }
    // Only pool managers create pools.
    const other = newAppPool();
    expect((await call(h, "POST", `/internal/v1/pools/${other.pool}`, { as: "auth" })).status).toBe(403);
    expect(await schemaExists(h.db, other.schema)).toBe(false);
    expect((await call(h, "DELETE", `/internal/v1/pools/${p.pool}`, { as: "worker" })).status).toBe(204);
  });
});
