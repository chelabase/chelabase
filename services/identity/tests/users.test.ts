import { afterAll, beforeAll, expect, it } from "vitest";
import { call, createPool, describeDb, newAppPool, startHarness, type Harness } from "./harness.ts";

describeDb("users", () => {
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
  const createUser = async (email: string, password?: string) => {
    const r = await call(h, "POST", `${base()}/users`, { body: { email, name: "U", ...(password ? { password } : {}) } });
    expect(r.status).toBe(201);
    return r.body.user as { id: string; email: string };
  };
  const signIn = (email: string, password: string) =>
    call(h, "POST", `${base()}/sessions`, { body: { email, password } });
  const lookup = (token: string) => call(h, "POST", `${base()}/sessions/lookup`, { body: { token } });

  // AC 3: a ban refuses sign-in and revokes every existing session at once.
  it("ban_blocks_sign_in_and_kills_sessions", async () => {
    const user = await createUser("ban@x.io", "password-ban-1");
    const s1 = await signIn("ban@x.io", "password-ban-1");
    const s2 = await signIn("ban@x.io", "password-ban-1");
    expect(s1.status).toBe(201);
    expect((await lookup(s1.body.session_token)).status).toBe(200);

    const banned = await call(h, "POST", `${base()}/users/${user.id}/ban`, { body: { reason: "abuse" } });
    expect(banned.status).toBe(200);
    expect(banned.body.user.banned).toBe(true);

    expect((await lookup(s1.body.session_token)).status).toBe(404);
    expect((await lookup(s2.body.session_token)).status).toBe(404);
    expect((await call(h, "GET", `${base()}/sessions/${s1.body.session_id}`)).status).toBe(404);
    const refused = await signIn("ban@x.io", "password-ban-1");
    expect(refused.status).toBe(403);
    expect(refused.body.error).toBe("banned");

    const unbanned = await call(h, "POST", `${base()}/users/${user.id}/unban`);
    expect(unbanned.status).toBe(200);
    expect(unbanned.body.user.banned).toBe(false);
    expect((await signIn("ban@x.io", "password-ban-1")).status).toBe(201);
  });

  it("admin_routes_round_trip", async () => {
    const user = await createUser("Round@X.io");
    expect(user.email).toBe("round@x.io");

    const got = await call(h, "GET", `${base()}/users/${user.id}`);
    expect(got.status).toBe(200);
    expect(got.body.user).toMatchObject({ id: user.id, email: "round@x.io", email_verified: false, banned: false });
    expect((await call(h, "GET", `${base()}/users/no-such-user`)).status).toBe(404);
    expect((await call(h, "GET", `${base()}/users/bad%20id`)).status).toBe(400);

    // A user created without a password cannot sign in until one is set.
    expect((await signIn("round@x.io", "password-round-1")).status).toBe(401);
    expect((await call(h, "PUT", `${base()}/users/${user.id}/password`, { body: { password: "short" } })).body.error).toBe(
      "weak_password",
    );
    expect((await call(h, "PUT", `${base()}/users/${user.id}/password`, { body: { password: "password-round-1" } })).status).toBe(200);
    expect((await signIn("round@x.io", "password-round-1")).status).toBe(201);
    expect((await call(h, "PUT", `${base()}/users/${user.id}/password`, { body: { password: "password-round-2" } })).status).toBe(200);
    expect((await signIn("round@x.io", "password-round-1")).status).toBe(401);
    expect((await signIn("round@x.io", "password-round-2")).status).toBe(201);

    const verified = await call(h, "POST", `${base()}/users/${user.id}/verify-email`);
    expect(verified.body.user.email_verified).toBe(true);

    // Changing the email clears verification unless the caller says otherwise.
    const moved = await call(h, "PUT", `${base()}/users/${user.id}/email`, { body: { email: "Moved@X.io" } });
    expect(moved.status).toBe(200);
    expect(moved.body.user).toMatchObject({ email: "moved@x.io", email_verified: false });
    expect((await call(h, "PUT", `${base()}/users/${user.id}/email`, { body: { email: "not an email" } })).body.error).toBe(
      "invalid_email",
    );
    const other = await createUser("taken@x.io");
    const clash = await call(h, "PUT", `${base()}/users/${other.id}/email`, { body: { email: "moved@x.io" } });
    expect(clash.status).toBe(409);

    // Paged list, oldest first.
    const page1 = await call(h, "GET", `${base()}/users?limit=1&offset=0`);
    expect(page1.status).toBe(200);
    expect(page1.body.users).toHaveLength(1);
    expect(page1.body.total).toBeGreaterThanOrEqual(3);
    const page2 = await call(h, "GET", `${base()}/users?limit=1&offset=1`);
    expect(page2.body.users[0].id).not.toBe(page1.body.users[0].id);
    expect((await call(h, "GET", `${base()}/users?limit=0`)).status).toBe(400);
    expect((await call(h, "GET", `${base()}/users?limit=1000`)).status).toBe(400);

    // The worker may read users.
    expect((await call(h, "GET", `${base()}/users/${user.id}`, { as: "worker" })).status).toBe(200);
    expect((await call(h, "GET", `${base()}/users`, { as: "worker" })).status).toBe(200);

    const session = await signIn("moved@x.io", "password-round-2");
    expect((await call(h, "DELETE", `${base()}/users/${user.id}`)).status).toBe(204);
    expect((await call(h, "GET", `${base()}/users/${user.id}`)).status).toBe(404);
    expect((await lookup(session.body.session_token)).status).toBe(404);
    expect((await call(h, "DELETE", `${base()}/users/${user.id}`)).status).toBe(404);
  });
});
