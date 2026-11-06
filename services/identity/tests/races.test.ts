// Races between sign-in, ban, revoke and concurrent writes. Database triggers stand in for
// latency at the exact point a race needs, so each case is deterministic. Every test uses its
// own pool, so its triggers touch nothing else.
import { afterAll, beforeAll, expect, it } from "vitest";
import { call, createPool, describeDb, newAppPool, startHarness, type Harness } from "./harness.ts";

describeDb("races", () => {
  let h: Harness;
  beforeAll(async () => {
    h = await startHarness();
  });
  afterAll(async () => {
    await h?.close();
  });

  async function freshPool() {
    const p = newAppPool();
    await createPool(h, p.pool);
    return { ...p, base: `/internal/v1/pools/${p.pool}` };
  }

  async function createUser(base: string, email: string, password: string): Promise<string> {
    const r = await call(h, "POST", `${base}/users`, { body: { email, password } });
    expect(r.status).toBe(201);
    return r.body.user.id;
  }

  async function sessionCount(schema: string, userId: string): Promise<number> {
    const r = await h.db.query(`SELECT count(*)::int AS n FROM ${schema}.session WHERE "userId" = $1`, [userId]);
    return r.rows[0].n;
  }

  /** A row trigger on `table` running `body` (plpgsql) at `when`. */
  async function trigger(schema: string, table: string, when: string, body: string): Promise<void> {
    const name = `t_${table}_${when.replaceAll(" ", "_").toLowerCase()}`;
    await h.db.query(`CREATE FUNCTION ${schema}.${name}() RETURNS trigger LANGUAGE plpgsql AS $$
      BEGIN ${body} RETURN NEW; END $$`);
    await h.db.query(
      `CREATE TRIGGER ${name} ${when} ON ${schema}."${table}" FOR EACH ROW EXECUTE FUNCTION ${schema}.${name}()`,
    );
  }

  // B1 (audit regression): a ban that lands between the admin plugin's banned check and the
  // session insert must not leave a live session.
  it("ban_during_sign_in_leaves_no_live_session", async () => {
    const p = await freshPool();
    const userId = await createUser(p.base, "slow@x.io", "password-slow-1");
    await trigger(p.schema, "session", "BEFORE INSERT", "PERFORM pg_sleep(1);");
    const signIn = call(h, "POST", `${p.base}/sessions`, { body: { email: "slow@x.io", password: "password-slow-1" } });
    await new Promise((r) => setTimeout(r, 600));
    expect((await call(h, "POST", `${p.base}/users/${userId}/ban`, { body: {} })).status).toBe(200);
    const s = await signIn;
    expect(s.status).toBe(403);
    expect(s.body.error).toBe("banned");
    expect(await sessionCount(p.schema, userId)).toBe(0);
  });

  // B1: a session row that survived for a banned user does not resolve, by token or by id,
  // and the row is deleted.
  it("banned_user_session_does_not_resolve", async () => {
    const p = await freshPool();
    const userId = await createUser(p.base, "late@x.io", "password-late-1");
    const sign = () => call(h, "POST", `${p.base}/sessions`, { body: { email: "late@x.io", password: "password-late-1" } });
    const a = await sign();
    const b = await sign();
    expect(a.status).toBe(201);
    // Ban without the route's session sweep, as a lost race would leave it.
    await h.db.query(`UPDATE ${p.schema}."user" SET banned = true WHERE id = $1`, [userId]);

    const byToken = await call(h, "POST", `${p.base}/sessions/lookup`, { body: { token: a.body.session_token } });
    expect(byToken.status).toBe(404);
    expect(byToken.body.error).toBe("session_not_found");
    const byId = await call(h, "GET", `${p.base}/sessions/${b.body.session_id}`);
    expect(byId.status).toBe(404);
    expect(byId.body.error).toBe("session_not_found");
    expect(await sessionCount(p.schema, userId)).toBe(0);
  });

  // N4: a revoke that deletes the new session between sign-in and its read-back answers 401.
  it("revoke_during_sign_in_answers_401", async () => {
    const p = await freshPool();
    const userId = await createUser(p.base, "gone@x.io", "password-gone-1");
    await trigger(p.schema, "session", "AFTER INSERT", `DELETE FROM ${p.schema}.session WHERE id = NEW.id;`);
    const s = await call(h, "POST", `${p.base}/sessions`, { body: { email: "gone@x.io", password: "password-gone-1" } });
    expect(s.status).toBe(401);
    expect(s.body.error).toBe("session_revoked");
    expect(await sessionCount(p.schema, userId)).toBe(0);
  });

  // N4: a ban that deletes the new session between sign-in and its read-back answers 403.
  it("ban_and_revoke_during_sign_in_answers_403", async () => {
    const p = await freshPool();
    const userId = await createUser(p.base, "both@x.io", "password-both-1");
    await trigger(
      p.schema,
      "session",
      "AFTER INSERT",
      `UPDATE ${p.schema}."user" SET banned = true WHERE id = NEW."userId";
       DELETE FROM ${p.schema}.session WHERE id = NEW.id;`,
    );
    const s = await call(h, "POST", `${p.base}/sessions`, { body: { email: "both@x.io", password: "password-both-1" } });
    expect(s.status).toBe(403);
    expect(s.body.error).toBe("banned");
    expect(await sessionCount(p.schema, userId)).toBe(0);
  });

  // S1: two creates of one email that both pass the existence check: one wins, one is 409.
  it("concurrent_create_same_email_conflicts", async () => {
    const p = await freshPool();
    await trigger(p.schema, "user", "BEFORE INSERT", "PERFORM pg_sleep(0.5);");
    const replies = await Promise.all(
      [1, 2, 3].map(() => call(h, "POST", `${p.base}/users`, { body: { email: "twin@x.io", password: "password-twin-1" } })),
    );
    const statuses = replies.map((r) => r.status).sort();
    expect(statuses).toEqual([201, 409, 409]);
    for (const r of replies.filter((r) => r.status === 409)) expect(r.body.error).toBe("conflict");
  });

  // S1: two users moved to one email at once: one wins, one is 409.
  it("concurrent_set_email_conflicts", async () => {
    const p = await freshPool();
    const a = await createUser(p.base, "a@x.io", "password-a-0001");
    const b = await createUser(p.base, "b@x.io", "password-b-0001");
    await trigger(p.schema, "user", "BEFORE UPDATE", "PERFORM pg_sleep(0.5);");
    const replies = await Promise.all(
      [a, b].map((id) => call(h, "PUT", `${p.base}/users/${id}/email`, { body: { email: "same@x.io" } })),
    );
    const statuses = replies.map((r) => r.status).sort();
    expect(statuses).toEqual([200, 409]);
    expect(replies.find((r) => r.status === 409)!.body.error).toBe("conflict");
  });
});
