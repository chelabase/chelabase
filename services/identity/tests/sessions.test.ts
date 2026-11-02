import { afterAll, beforeAll, expect, it } from "vitest";
import { call, createPool, describeDb, newAppPool, startHarness, type Harness } from "./harness.ts";

describeDb("sessions", () => {
  let h: Harness;
  let pool: string;
  beforeAll(async () => {
    h = await startHarness();
    pool = newAppPool().pool;
    await createPool(h, pool);
  });
  afterAll(async () => {
    await h?.close();
  });

  const base = () => `/internal/v1/pools/${pool}`;
  const signIn = async (email: string, password: string) => {
    const r = await call(h, "POST", `${base()}/sessions`, { body: { email, password } });
    expect(r.status).toBe(201);
    return r.body as { session_id: string; session_token: string; user: { id: string } };
  };
  const lookup = (token: string) => call(h, "POST", `${base()}/sessions/lookup`, { body: { token } });

  // AC 4: revoking all of a user's sessions makes every old token resolve to nothing.
  it("revoke_all", async () => {
    await call(h, "POST", `${base()}/users`, { body: { email: "all@x.io", password: "password-all-1" } });
    await call(h, "POST", `${base()}/users`, { body: { email: "keep@x.io", password: "password-keep-1" } });
    const a = await signIn("all@x.io", "password-all-1");
    const b = await signIn("all@x.io", "password-all-1");
    const other = await signIn("keep@x.io", "password-keep-1");

    const revoked = await call(h, "DELETE", `${base()}/users/${a.user.id}/sessions`);
    expect(revoked.status).toBe(204);
    expect((await lookup(a.session_token)).status).toBe(404);
    expect((await lookup(b.session_token)).status).toBe(404);
    expect((await call(h, "GET", `${base()}/sessions/${a.session_id}`)).status).toBe(404);
    // Another user's session is untouched.
    expect((await lookup(other.session_token)).status).toBe(200);
    // The user can still sign in again.
    expect((await lookup((await signIn("all@x.io", "password-all-1")).session_token)).status).toBe(200);
  });

  it("get_and_revoke_one", async () => {
    await call(h, "POST", `${base()}/users`, { body: { email: "one@x.io", password: "password-one-1" } });
    const a = await signIn("one@x.io", "password-one-1");
    const b = await signIn("one@x.io", "password-one-1");

    const got = await call(h, "GET", `${base()}/sessions/${a.session_id}`);
    expect(got.status).toBe(200);
    expect(got.body.session).toMatchObject({ id: a.session_id, user_id: a.user.id });
    expect(got.body.user.email).toBe("one@x.io");
    // The id lookup never hands out the token.
    expect(JSON.stringify(got.body)).not.toContain(a.session_token);

    expect((await call(h, "DELETE", `${base()}/sessions/${a.session_id}`)).status).toBe(204);
    expect((await lookup(a.session_token)).status).toBe(404);
    expect((await call(h, "GET", `${base()}/sessions/${a.session_id}`)).status).toBe(404);
    expect((await lookup(b.session_token)).status).toBe(200);
    expect((await call(h, "DELETE", `${base()}/sessions/${a.session_id}`)).status).toBe(404);
  });

  it("a session id from another pool is not found", async () => {
    const other = newAppPool().pool;
    await createPool(h, other);
    await call(h, "POST", `${base()}/users`, { body: { email: "cross@x.io", password: "password-cross-1" } });
    const a = await signIn("cross@x.io", "password-cross-1");
    expect((await call(h, "GET", `/internal/v1/pools/${other}/sessions/${a.session_id}`)).status).toBe(404);
    expect((await call(h, "DELETE", `/internal/v1/pools/${other}/sessions/${a.session_id}`)).status).toBe(404);
    expect((await lookup(a.session_token)).status).toBe(200);
  });
});
