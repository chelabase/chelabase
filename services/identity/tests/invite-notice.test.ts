// The invite notice (the code-less mail to an existing account) and what `users.get` tells the
// orchestrator about an account.
import { afterAll, beforeAll, expect, it } from "vitest";
import {
  call,
  createPool,
  describeDb,
  newAppPool,
  startHarness,
  startSmtpSink,
  type Harness,
  type SmtpSink,
} from "./harness.ts";

const LINK = "https://studio.test/accept-invite?t=ticket-abc";

describeDb("invite notice", () => {
  let h: Harness;
  let mailed: Harness;
  let sink: SmtpSink;
  const base = "/internal/v1/pools/members";
  beforeAll(async () => {
    h = await startHarness();
    sink = await startSmtpSink();
    mailed = await startHarness({
      smtp: { host: "127.0.0.1", port: sink.port, tls: "none", from: "Chelabase <noreply@chelabase.test>" },
    });
    await createPool(h, "members");
  });
  afterAll(async () => {
    await h?.close();
    await mailed?.close();
    await sink?.close();
  });

  let n = 0;
  const createUser = async (password?: string) => {
    const email = `notice${++n}-${Date.now()}@x.io`;
    const r = await call(h, "POST", `${base}/users`, { body: { email, ...(password ? { password } : {}) } });
    expect(r.status).toBe(201);
    return r.body.user as { id: string; email: string };
  };
  const notice = (hh: Harness, id: string, body: unknown, as: "orchestrator" | "auth" | "worker" = "orchestrator", b = base) =>
    call(hh, "POST", `${b}/users/${id}/invite-notice`, { as, body: body as Record<string, unknown> });

  it("notice_carries_the_link_and_no_code", async () => {
    const user = await createUser("password-notice-1");
    const r = await notice(mailed, user.id, { project_name: "Acme", link: LINK });
    expect(r.status, JSON.stringify(r.body)).toBe(202);
    expect(r.body).toEqual({ sent: true });
    const message = sink.messages.at(-1)!;
    expect(message.to).toEqual([user.email]);
    const decoded = message.data.replace(/=\r\n/g, "").replace(/=3D/g, "=");
    expect(decoded).toContain("ticket-abc");
    expect(decoded).toContain("Acme");
    expect(decoded).toMatch(/your own password|your existing password/i);
    // Neither a code nor an unfilled placeholder, and no code row was written.
    expect(decoded).not.toContain("{{");
    expect(decoded.split("\r\n\r\n").slice(1).join("\n")).not.toMatch(/\b[0-9]{6}\b/);
    const rows = await h.db.query(`SELECT 1 FROM identity_members.email_code WHERE "userId" = $1`, [user.id]);
    expect(rows.rowCount).toBe(0);
  });

  it("notice_refuses_a_link_with_a_code", async () => {
    const user = await createUser("password-notice-2");
    const r = await notice(mailed, user.id, { link: `${LINK}&code=123456` });
    expect(r.status).toBe(400);
    expect(r.body.error).toBe("invalid_request");
  });

  it("notice_needs_a_link", async () => {
    const user = await createUser("password-notice-3");
    expect((await notice(mailed, user.id, {})).status).toBe(400);
  });

  it("notice_is_members_only", async () => {
    const pool = newAppPool().pool;
    await createPool(h, pool);
    // The orchestrator's allow-list refuses an app pool before the route's own check.
    const r = await notice(mailed, "00000000-0000-4000-8000-000000000000", { link: LINK }, "orchestrator", `/internal/v1/pools/${pool}`);
    expect(r.status).toBe(403);
    expect(r.body.error).toBe("forbidden");
  });

  it("notice_without_smtp_is_409", async () => {
    const user = await createUser("password-notice-4");
    const r = await notice(h, user.id, { link: LINK });
    expect(r.status).toBe(409);
    expect(r.body.error).toBe("email_not_configured");
  });

  it("notice_for_an_unknown_user_is_404", async () => {
    const r = await notice(mailed, "00000000-0000-4000-8000-000000000000", { link: LINK });
    expect(r.status).toBe(404);
    expect(r.body.error).toBe("user_not_found");
  });

  it("notice_only_the_orchestrator_may_send", async () => {
    const user = await createUser("password-notice-5");
    expect((await notice(mailed, user.id, { link: LINK }, "auth")).status).toBe(403);
    expect((await notice(mailed, user.id, { link: LINK }, "worker")).status).toBe(403);
  });

  it("user_read_tells_password_and_ban_reason", async () => {
    const withPassword = await createUser("password-read-1");
    const without = await createUser();
    const read = async (id: string) => (await call(h, "GET", `${base}/users/${id}`, { as: "orchestrator" })).body.user;
    expect(await read(withPassword.id)).toMatchObject({ has_password: true, ban_reason: null });
    expect(await read(without.id)).toMatchObject({ has_password: false, ban_reason: null });
    await call(h, "POST", `${base}/users/${without.id}/ban`, { as: "orchestrator", body: { reason: "chelabase:test" } });
    expect(await read(without.id)).toMatchObject({ banned: true, ban_reason: "chelabase:test" });
    await call(h, "POST", `${base}/users/${without.id}/unban`, { as: "orchestrator" });
    expect(await read(without.id)).toMatchObject({ banned: false, ban_reason: null });
  });
});
