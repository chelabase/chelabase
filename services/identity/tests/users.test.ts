import { afterAll, beforeAll, expect, it } from "vitest";
import { call, createPool, describeDb, newAppPool, startHarness, startSmtpSink, type Harness, type SmtpSink } from "./harness.ts";

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

  it("users.set_password ends the user's sessions", async () => {
    const user = await createUser("pwsess@x.io", "password-pwsess-1");
    const other = await createUser("pwsess-other@x.io", "password-other-1");
    const s1 = await signIn("pwsess@x.io", "password-pwsess-1");
    const s2 = await signIn("pwsess@x.io", "password-pwsess-1");
    const o1 = await signIn("pwsess-other@x.io", "password-other-1");
    expect((await lookup(s1.body.session_token)).status).toBe(200);

    const r = await call(h, "PUT", `${base()}/users/${user.id}/password`, { body: { password: "password-pwsess-2" } });
    expect(r.status).toBe(200);
    expect((await lookup(s1.body.session_token)).status).toBe(404);
    expect((await lookup(s2.body.session_token)).status).toBe(404);
    // Another user's session is untouched.
    expect((await lookup(o1.body.session_token)).status).toBe(200);
    expect(other.id).not.toBe(user.id);
  });

  it("users.set_password is atomic: a failed session delete keeps the old password", async () => {
    const user = await createUser("pwatomic@x.io", "password-atomic-1");
    const s1 = await signIn("pwatomic@x.io", "password-atomic-1");
    expect(s1.status).toBe(201);
    const schema = `identity_app_${pool.slice("app-".length).replaceAll("-", "")}`;
    const q = (text: string) => h.db.query(text);
    const fn = `"${schema}".fail_session_delete`;
    await q(`CREATE FUNCTION ${fn}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected'; END $$`);
    await q(`CREATE TRIGGER fail_session_delete BEFORE DELETE ON "${schema}"."session" FOR EACH ROW EXECUTE FUNCTION ${fn}()`);
    try {
      const r = await call(h, "PUT", `${base()}/users/${user.id}/password`, { body: { password: "password-atomic-2" } });
      expect(r.status).toBe(500);
    } finally {
      await q(`DROP TRIGGER fail_session_delete ON "${schema}"."session"`);
      await q(`DROP FUNCTION ${fn}()`);
    }
    // Nothing changed: the old password still signs in, the new one does not, the session lives.
    expect((await signIn("pwatomic@x.io", "password-atomic-2")).status).toBe(401);
    expect((await signIn("pwatomic@x.io", "password-atomic-1")).status).toBe(201);
    expect((await lookup(s1.body.session_token)).status).toBe(200);
  });

  it("users.create refuses a short password", async () => {
    const r = await call(h, "POST", `${base()}/users`, { body: { email: "shortpw@x.io", name: "U", password: "short" } });
    expect(r.status).toBe(400);
    expect(r.body.error).toBe("weak_password");
    const found = await call(h, "GET", `${base()}/users?email=shortpw%40x.io`);
    expect(found.status).toBe(200);
    expect(found.body.users).toEqual([]);
  });

  it("users.create without a password still works", async () => {
    const user = await createUser("nopw@x.io");
    expect(user.email).toBe("nopw@x.io");
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

  // The platform keys app users by UUID (users tables, app_sessions, the sync job).
  it("user_and_session_ids_are_uuids", async () => {
    const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
    const user = await createUser("uuid@x.io", "password-uuid-1");
    expect(user.id).toMatch(uuid);
    const s = await signIn("uuid@x.io", "password-uuid-1");
    expect(s.body.session_id).toMatch(uuid);
    // An id that isn't a UUID is not found, never a database error.
    expect((await call(h, "GET", `${base()}/users/abcDEF123`)).body.error).toBe("user_not_found");
    expect((await call(h, "POST", `${base()}/users/abcDEF123/ban`)).body.error).toBe("user_not_found");
    expect((await call(h, "DELETE", `${base()}/users/abcDEF123/sessions`)).body.error).toBe("user_not_found");
    expect((await call(h, "GET", `${base()}/sessions/abcDEF123`)).body.error).toBe("session_not_found");
    expect((await call(h, "DELETE", `${base()}/sessions/abcDEF123`)).body.error).toBe("session_not_found");
  });

  // `search` matches part of the email, ignoring case; LIKE wildcards are literal.
  it("list_search_matches_part_of_the_email", async () => {
    const a = await createUser("Find.Me+one@search.io");
    const b = await createUser("find_me2@search.io");
    await createUser("other@search.io");
    const ids = async (q: string) => {
      const r = await call(h, "GET", `${base()}/users?search=${encodeURIComponent(q)}&limit=200`, { as: "orchestrator" });
      expect(r.status, q).toBe(200);
      expect(r.body.total, q).toBe(r.body.users.length);
      return (r.body.users as { id: string }[]).map((u) => u.id).sort();
    };
    expect(await ids("FIND")).toEqual([a.id, b.id].sort());
    expect(await ids("me+ONE@")).toEqual([a.id]);
    expect(await ids("_")).toEqual([b.id]);
    expect(await ids("%")).toEqual([]);
    expect(await ids("find%me")).toEqual([]);
    expect((await call(h, "GET", `${base()}/users?search=${"x".repeat(321)}`)).status).toBe(400);
  });

  // `email` is an exact match on the (lowercase, unique) address: at most one user, one read,
  // however many other addresses contain it.
  it("list_email_matches_exactly_one_user", async () => {
    const a = await createUser("Exact@Lookup.io");
    await createUser("anexact@lookup.io");
    await createUser("exact@lookup.io.evil");
    const find = async (q: string, as: "auth" | "orchestrator" = "auth") => {
      const r = await call(h, "GET", `${base()}/users?email=${encodeURIComponent(q)}`, { as });
      expect(r.status, q).toBe(200);
      expect(r.body.total, q).toBe(r.body.users.length);
      return (r.body.users as { id: string }[]).map((u) => u.id);
    };
    expect(await find("exact@lookup.io")).toEqual([a.id]);
    expect(await find("EXACT@lookup.IO", "orchestrator")).toEqual([a.id]);
    expect(await find("xact@lookup.io")).toEqual([]);
    expect(await find("%@lookup.io")).toEqual([]);
    expect(await find("")).toEqual([]);
    expect((await call(h, "GET", `${base()}/users?email=${"x".repeat(321)}`)).status).toBe(400);
    expect((await call(h, "GET", `${base()}/users?email=a%40b.io&search=a`)).status).toBe(400);
  });

  // The previous address hears of a change only when the caller asks for it.
  describeDb("set_email notices", () => {
    let mailed: Harness;
    let sink: SmtpSink;
    let p: string;
    beforeAll(async () => {
      sink = await startSmtpSink();
      mailed = await startHarness({
        smtp: { host: "127.0.0.1", port: sink.port, tls: "none", from: "Chelabase <noreply@chelabase.test>" },
      });
      p = newAppPool().pool;
      await createPool(mailed, p);
    });
    afterAll(async () => {
      await mailed?.close();
      await sink?.close();
    });
    let n = 0;
    const at = (tag: string) => `${tag}-${++n}-${Date.now()}@x.io`;
    const setEmail = async (id: string, body: Record<string, unknown>) => {
      const r = await call(mailed, "PUT", `/internal/v1/pools/${p}/users/${id}/email`, { body });
      expect(r.status, JSON.stringify(r.body)).toBe(200);
      return r.body.user as { email: string };
    };
    const mkUser = async () => {
      const r = await call(mailed, "POST", `/internal/v1/pools/${p}/users`, { body: { email: at("old") } });
      expect(r.status).toBe(201);
      return r.body.user as { id: string; email: string };
    };

    it("set_email notify_previous mails the old address", async () => {
      const user = await mkUser();
      const next = at("new");
      const before = sink.messages.length;
      expect((await setEmail(user.id, { email: next, email_verified: true, notify_previous: true })).email).toBe(next);
      expect(sink.messages.length).toBe(before + 1);
      const notice = sink.messages.at(-1)!;
      expect(notice.to).toEqual([user.email]);
      expect(notice.data).toContain("was changed");
      // Neutral advice (A4): ask the app's support, not a password reset.
      expect(notice.data).toContain("contact");
      expect(notice.data).not.toMatch(/reset your password/i);
      expect(notice.data).not.toContain(next);
      // The same address again is no change, and mails nothing.
      await setEmail(user.id, { email: next, notify_previous: true });
      expect(sink.messages.length).toBe(before + 1);
      expect((await call(mailed, "PUT", `/internal/v1/pools/${p}/users/${user.id}/email`, { body: { email: at("x"), notify_previous: "yes" } })).status).toBe(400);
    });

    it("set_email notice names the project", async () => {
      const user = await mkUser();
      const before = sink.messages.length;
      await setEmail(user.id, { email: at("new"), notify_previous: true, project_name: "Acme" });
      expect(sink.messages.length).toBe(before + 1);
      expect(sink.messages.at(-1)!.data).toContain("Your Acme email address was changed");
      // A name identity cannot show (not one line, or blank once JavaScript's trim strips
      // U+FEFF too) never blocks the change or the mail: the notice names the default.
      for (const bad of ["Acme\nInc", "\ufeff", "\ufeff ", " \u00a0 "]) {
        const n = sink.messages.length;
        const r = await call(mailed, "PUT", `/internal/v1/pools/${p}/users/${(await mkUser()).id}/email`, {
          body: { email: at(`bad${n}`), notify_previous: true, project_name: bad },
        });
        expect(r.status, JSON.stringify(bad)).toBe(200);
        expect(sink.messages.length, JSON.stringify(bad)).toBe(n + 1);
        expect(sink.messages.at(-1)!.data, JSON.stringify(bad)).toContain("Your Chelabase email address was changed");
      }
    });

    it("set_email without notify_previous mails nothing", async () => {
      const user = await mkUser();
      const before = sink.messages.length;
      await setEmail(user.id, { email: at("new") });
      await setEmail(user.id, { email: at("new"), notify_previous: false });
      expect(sink.messages.length).toBe(before);
    });
  });
});
