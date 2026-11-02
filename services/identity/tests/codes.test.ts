// Email codes: issue, deliver and consume verify, reset and invite codes.
import { afterAll, afterEach, beforeAll, expect, it, vi } from "vitest";
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

const MINUTE = 60_000;
const DAY = 24 * 60 * MINUTE;

describeDb("codes", () => {
  let h: Harness;
  let mailed: Harness;
  let sink: SmtpSink;
  let pool: string;
  beforeAll(async () => {
    h = await startHarness();
    sink = await startSmtpSink();
    mailed = await startHarness({
      smtp: { host: "127.0.0.1", port: sink.port, tls: "none", from: "Chelabase <noreply@chelabase.test>" },
    });
    pool = newAppPool().pool;
    await createPool(h, pool);
  });
  afterAll(async () => {
    await h?.close();
    await mailed?.close();
    await sink?.close();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  const base = (p = pool) => `/internal/v1/pools/${p}`;
  let n = 0;
  const createUser = async (password?: string, p = pool, email = `u${++n}-${Date.now()}@x.io`) => {
    const r = await call(h, "POST", `${base(p)}/users`, { body: { email, ...(password ? { password } : {}) } });
    expect(r.status).toBe(201);
    return r.body.user as { id: string; email: string; email_verified: boolean };
  };
  const issue = async (userId: string, kind: string, p = pool) => {
    const r = await call(h, "POST", `${base(p)}/codes`, { body: { user_id: userId, kind, delivery: "return" } });
    expect(r.status, JSON.stringify(r.body)).toBe(201);
    expect(r.body.code).toMatch(/^[0-9]{6}$/);
    return r.body as { code: string; kind: string; expires_at: string };
  };
  const verify = (userId: string, kind: string, code: string, extra: Record<string, unknown> = {}, p = pool, hh = h) =>
    call(hh, "POST", `${base(p)}/codes/verify`, { body: { user_id: userId, kind, code, ...extra } });
  const wrong = (code: string, k = 1) => String((Number(code) + k) % 1_000_000).padStart(6, "0");
  const signIn = (email: string, password: string, p = pool) =>
    call(h, "POST", `${base(p)}/sessions`, { body: { email, password } });
  const lookup = (token: string) => call(h, "POST", `${base()}/sessions/lookup`, { body: { token } });

  // AC 1
  it("verify_single_use", async () => {
    const user = await createUser();
    const { code } = await issue(user.id, "verify");
    const ok = await verify(user.id, "verify", code);
    expect(ok.status).toBe(200);
    expect(ok.body.user).toMatchObject({ id: user.id, email_verified: true });
    const again = await verify(user.id, "verify", code);
    expect(again.status).toBe(400);
    expect(again.body.error).toBe("invalid_code");
  });

  // AC 2
  it("lockout_after_five", async () => {
    const user = await createUser("password-lock-1");
    const { code } = await issue(user.id, "reset");
    for (let i = 1; i <= 5; i++) {
      const r = await verify(user.id, "reset", wrong(code, i), { new_password: "password-lock-2" });
      expect(r.status).toBe(400);
      expect(r.body.error).toBe("invalid_code");
    }
    const right = await verify(user.id, "reset", code, { new_password: "password-lock-2" });
    expect(right.status).toBe(400);
    expect(right.body.error).toBe("invalid_code");
    expect((await signIn(user.email, "password-lock-2")).status).toBe(401);
    // A new code is required, and works.
    const fresh = await issue(user.id, "reset");
    expect((await verify(user.id, "reset", fresh.code, { new_password: "password-lock-2" })).status).toBe(200);
  });

  // AC 3
  it("expired_refused", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(Date.now());
    const user = await createUser();
    const { code } = await issue(user.id, "verify");
    vi.setSystemTime(Date.now() + 15 * MINUTE + 1000);
    const r = await verify(user.id, "verify", code);
    expect(r.status).toBe(400);
    expect(r.body.error).toBe("invalid_code");
  });

  // AC 4
  it("cross_pool_refused", async () => {
    const other = newAppPool().pool;
    await createPool(h, other);
    const email = `cross-${Date.now()}@x.io`;
    const a = await createUser("password-cross-1", pool, email);
    const b = await createUser("password-cross-1", other, email);
    const { code } = await issue(a.id, "reset");
    // Same user id in the other pool, and the other pool's user with the same email.
    expect((await verify(a.id, "reset", code, { new_password: "password-cross-2" }, other)).body.error).toBe("invalid_code");
    const r = await verify(b.id, "reset", code, { new_password: "password-cross-2" }, other);
    expect(r.status).toBe(400);
    expect(r.body.error).toBe("invalid_code");
    expect((await signIn(email, "password-cross-2", other)).status).toBe(401);
    // The code still works where it was issued.
    expect((await verify(a.id, "reset", code, { new_password: "password-cross-2" })).status).toBe(200);
  });

  // AC 5
  it("return_vs_email_delivery", async () => {
    const logged: string[] = [];
    const capture = (...args: unknown[]) => void logged.push(args.map(String).join(" "));
    for (const m of ["log", "info", "warn", "error", "debug"] as const) vi.spyOn(console, m).mockImplementation(capture);
    const out = process.stdout.write.bind(process.stdout);
    const err = process.stderr.write.bind(process.stderr);
    vi.spyOn(process.stdout, "write").mockImplementation((c: any, ...rest: any[]) => (logged.push(String(c)), out(c, ...rest)));
    vi.spyOn(process.stderr, "write").mockImplementation((c: any, ...rest: any[]) => (logged.push(String(c)), err(c, ...rest)));

    const p = newAppPool().pool;
    await createPool(mailed, p);
    const created = await call(mailed, "POST", `${base(p)}/users`, { body: { email: "deliver@x.io" } });
    const user = created.body.user;

    const before = sink.messages.length;
    const returned = await call(mailed, "POST", `${base(p)}/codes`, {
      body: { user_id: user.id, kind: "verify", delivery: "return" },
    });
    expect(returned.status).toBe(201);
    expect(returned.body.code).toMatch(/^[0-9]{6}$/);
    expect(sink.messages.length).toBe(before);

    const emailed = await call(mailed, "POST", `${base(p)}/codes`, {
      body: { user_id: user.id, kind: "verify", delivery: "email", project_name: "Acme", link: "https://acme.test/verify" },
    });
    expect(emailed.status).toBe(201);
    expect(emailed.body.code).toBeUndefined();
    expect(sink.messages.length).toBe(before + 1);
    const message = sink.messages.at(-1)!;
    expect(message.to).toEqual(["deliver@x.io"]);
    expect(message.data).toContain("Acme");
    expect(message.data).toContain("https://acme.test/verify");
    // The code as the text part shows it, after "code is:" (not any six digits in the raw DATA).
    const textPart = message.data.split(/Content-Type: text\/plain/i)[1]!;
    const code = /code is:\s+([0-9]{6})\b/.exec(textPart)![1]!;
    expect(JSON.stringify(emailed.body)).not.toContain(code);
    expect(logged.join("\n")).not.toContain(code);
    // The emailed code replaced the returned one.
    expect((await verify(user.id, "verify", returned.body.code, {}, p, mailed)).status).toBe(400);
    expect((await verify(user.id, "verify", code, {}, p, mailed)).status).toBe(200);
    expect(logged.join("\n")).not.toContain(code);

    const status = await call(mailed, "GET", "/internal/v1/email/status");
    expect(status.status).toBe(200);
    expect(status.body).toEqual({ configured: true, reachable: true });
  });

  // AC 6
  it("no_smtp_409", async () => {
    const status = await call(h, "GET", "/internal/v1/email/status");
    expect(status.status).toBe(200);
    expect(status.body).toEqual({ configured: false, reachable: false });
    const user = await createUser();
    const r = await call(h, "POST", `${base()}/codes`, { body: { user_id: user.id, kind: "verify", delivery: "email" } });
    expect(r.status).toBe(409);
    expect(r.body.error).toBe("email_not_configured");
  });

  // AC 7
  it("reset_revokes_sessions", async () => {
    const user = await createUser("password-old-1");
    const s1 = await signIn(user.email, "password-old-1");
    const s2 = await signIn(user.email, "password-old-1");
    expect(s1.status).toBe(201);
    const { code } = await issue(user.id, "reset");
    // A too-short password is refused before the code is spent.
    expect((await verify(user.id, "reset", code, { new_password: "short" })).body.error).toBe("weak_password");
    expect((await verify(user.id, "reset", code)).status).toBe(400);
    const r = await verify(user.id, "reset", code, { new_password: "password-new-1" });
    expect(r.status).toBe(200);
    expect((await lookup(s1.body.session_token)).status).toBe(404);
    expect((await lookup(s2.body.session_token)).status).toBe(404);
    expect((await signIn(user.email, "password-old-1")).status).toBe(401);
    expect((await signIn(user.email, "password-new-1")).status).toBe(201);
  });

  it("parallel_guesses_capped", async () => {
    const user = await createUser("password-par-1");
    const { code } = await issue(user.id, "reset");
    const guesses = await Promise.all(
      Array.from({ length: 50 }, (_, i) => verify(user.id, "reset", wrong(code, i + 1), { new_password: "password-par-2" })),
    );
    expect(guesses.every((g) => g.status === 400)).toBe(true);
    expect((await verify(user.id, "reset", code, { new_password: "password-par-2" })).status).toBe(400);
  });

  it("parallel_correct_single_success", async () => {
    const user = await createUser();
    const { code } = await issue(user.id, "verify");
    const tries = await Promise.all(Array.from({ length: 10 }, () => verify(user.id, "verify", code)));
    expect(tries.filter((t) => t.status === 200)).toHaveLength(1);
    expect(tries.filter((t) => t.status === 400)).toHaveLength(9);
  });

  it("reset_does_not_clobber_invite", async () => {
    const user = await createUser();
    const invite = await issue(user.id, "invite");
    await issue(user.id, "reset");
    // A verify code is not an invite code, and an invite code is not a verify code.
    expect((await verify(user.id, "verify", invite.code)).status).toBe(400);
    const r = await verify(user.id, "invite", invite.code, { new_password: "password-inv-1" });
    expect(r.status).toBe(200);
    expect(r.body.user.email_verified).toBe(true);
    expect((await signIn(user.email, "password-inv-1")).status).toBe(201);
  });

  it("ttl_per_kind", async () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const t0 = Date.now();
    vi.setSystemTime(t0);
    const a = await createUser();
    const b = await createUser();
    const reset = await issue(a.id, "reset");
    const verifyCode = await issue(a.id, "verify");
    const inviteA = await issue(a.id, "invite");
    const inviteB = await issue(b.id, "invite");
    expect(Date.parse(reset.expires_at)).toBe(t0 + 15 * MINUTE);
    expect(Date.parse(verifyCode.expires_at)).toBe(t0 + 15 * MINUTE);
    expect(Date.parse(inviteA.expires_at)).toBe(t0 + 7 * DAY);

    vi.setSystemTime(t0 + 15 * MINUTE + 1000);
    expect((await verify(a.id, "reset", reset.code, { new_password: "password-ttl-1" })).status).toBe(400);
    expect((await verify(a.id, "verify", verifyCode.code)).status).toBe(400);
    expect((await verify(a.id, "invite", inviteA.code, { new_password: "password-ttl-1" })).status).toBe(200);

    vi.setSystemTime(t0 + 7 * DAY + 1000);
    expect((await verify(b.id, "invite", inviteB.code, { new_password: "password-ttl-2" })).status).toBe(400);
  });

  // B1: a code proves control of the address it was mailed to, so changing the email kills it.
  it("email_change_kills_codes", async () => {
    const changeEmail = async (id: string, extra: Record<string, unknown> = {}) => {
      const r = await call(h, "PUT", `${base()}/users/${id}/email`, { body: { email: `moved-${++n}-${Date.now()}@y.io`, ...extra } });
      expect(r.status, JSON.stringify(r.body)).toBe(200);
      return r.body.user as { id: string; email: string; email_verified: boolean };
    };

    // verify: the old mailbox's code must not mark the new address verified.
    const v = await createUser();
    const verifyCode = await issue(v.id, "verify");
    expect((await changeEmail(v.id)).email_verified).toBe(false);
    const rv = await verify(v.id, "verify", verifyCode.code);
    expect(rv.status, JSON.stringify(rv.body)).toBe(400);
    expect(rv.body.error).toBe("invalid_code");
    const fresh = await issue(v.id, "verify");
    const okV = await verify(v.id, "verify", fresh.code);
    expect(okV.status).toBe(200);
    expect(okV.body.user.email_verified).toBe(true);

    // reset: the old mailbox's code must not set the password.
    const r = await createUser("password-move-0");
    const resetCode = await issue(r.id, "reset");
    const moved = await changeEmail(r.id, { email_verified: true });
    const rr = await verify(r.id, "reset", resetCode.code, { new_password: "password-move-9" });
    expect(rr.status, JSON.stringify(rr.body)).toBe(400);
    expect(rr.body.error).toBe("invalid_code");
    expect((await signIn(moved.email, "password-move-9")).status).toBe(401);
    expect((await signIn(moved.email, "password-move-0")).status).toBe(201);

    // invite: a code sent to a mistyped address dies when the address is corrected.
    const i = await createUser();
    const inviteCode = await issue(i.id, "invite");
    await changeEmail(i.id);
    const ri = await verify(i.id, "invite", inviteCode.code, { new_password: "password-move-8" });
    expect(ri.status, JSON.stringify(ri.body)).toBe(400);
    expect(ri.body.error).toBe("invalid_code");
    const again = await issue(i.id, "invite");
    expect((await verify(i.id, "invite", again.code, { new_password: "password-move-8" })).status).toBe(200);
  });

  // S1: an invite never overwrites an established account (a password and a verified email).
  it("invite_refused_for_user_with_password", async () => {
    const user = await createUser("password-est-0");
    expect((await call(h, "POST", `${base()}/users/${user.id}/verify-email`)).status).toBe(200);
    for (const as of ["auth", "orchestrator"] as const) {
      const r = await call(h, "POST", `${base()}/codes`, { as, body: { user_id: user.id, kind: "invite", delivery: "return" } });
      expect(r.status, JSON.stringify(r.body)).toBe(409);
      expect(r.body.error).toBe("invite_refused");
      expect(r.body.code).toBeUndefined();
    }
    // Other kinds are unaffected.
    expect((await call(h, "POST", `${base()}/codes`, { body: { user_id: user.id, kind: "reset", delivery: "return" } })).status).toBe(201);
    expect((await signIn(user.email, "password-est-0")).status).toBe(201);
  });

  it("invite_refused_at_verify_once_established", async () => {
    // Invited while pending (no password), then the account was set up some other way.
    const user = await createUser();
    const { code } = await issue(user.id, "invite");
    expect((await call(h, "PUT", `${base()}/users/${user.id}/password`, { body: { password: "password-est-1" } })).status).toBe(200);
    expect((await call(h, "POST", `${base()}/users/${user.id}/verify-email`)).status).toBe(200);
    const wrongReply = await verify(user.id, "invite", wrong(code), { new_password: "password-est-2" });
    // More refusals than MAX_ATTEMPTS: none of them may cost an attempt, and each looks like a wrong code.
    for (let k = 0; k < 6; k++) {
      const r = await verify(user.id, "invite", code, { new_password: "password-est-2" });
      expect(r.status, JSON.stringify(r.body)).toBe(400);
      expect(r.body).toEqual(wrongReply.body);
    }
    expect((await signIn(user.email, "password-est-2")).status).toBe(401);
    expect((await signIn(user.email, "password-est-1")).status).toBe(201);
    // Back to unverified (same address, so the code's binding holds): the code was never spent.
    const unverify = await call(h, "PUT", `${base()}/users/${user.id}/email`, { body: { email: user.email } });
    expect(unverify.status).toBe(200);
    expect(unverify.body.user.email_verified).toBe(false);
    expect((await verify(user.id, "invite", code, { new_password: "password-est-2" })).status).toBe(200);
  });

  it("invite_accepted_for_preregistered_user", async () => {
    // A password but an unverified email: a pre-registered account the invite completes.
    const user = await createUser("password-pre-0");
    const s = await signIn(user.email, "password-pre-0");
    expect(s.status).toBe(201);
    const { code } = await issue(user.id, "invite", pool);
    const orch = await call(h, "POST", `${base()}/codes`, { as: "orchestrator", body: { user_id: user.id, kind: "invite", delivery: "return" } });
    expect(orch.status).toBe(201);
    // The orchestrator's code replaced the first.
    expect((await verify(user.id, "invite", code, { new_password: "password-pre-1" })).status).toBe(400);
    const r = await verify(user.id, "invite", orch.body.code, { new_password: "password-pre-1" });
    expect(r.status).toBe(200);
    expect(r.body.user.email_verified).toBe(true);
    expect((await lookup(s.body.session_token)).status).toBe(404);
    expect((await signIn(user.email, "password-pre-0")).status).toBe(401);
    expect((await signIn(user.email, "password-pre-1")).status).toBe(201);
  });

  it("caller_allow_list", async () => {
    const user = await createUser();
    const create = (as: "auth" | "orchestrator" | "worker", kind: string) =>
      call(h, "POST", `${base()}/codes`, { as, body: { user_id: user.id, kind, delivery: "return" } });
    expect((await create("orchestrator", "invite")).status).toBe(201);
    expect((await create("orchestrator", "reset")).status).toBe(403);
    expect((await create("orchestrator", "verify")).status).toBe(403);
    expect((await create("worker", "invite")).status).toBe(403);
    expect((await call(h, "POST", `${base()}/codes/verify`, { as: "orchestrator", body: {} })).status).toBe(403);
    expect((await call(h, "GET", "/internal/v1/email/status", { as: "orchestrator" })).status).toBe(200);
    expect((await call(h, "GET", "/internal/v1/email/status", { as: "worker" })).status).toBe(403);
  });

  it("rejects_bad_input", async () => {
    const user = await createUser();
    const bad = [
      { user_id: user.id, kind: "magic", delivery: "return" },
      { user_id: user.id, kind: "verify", delivery: "carrier-pigeon" },
      { user_id: "bad id", kind: "verify", delivery: "return" },
      { user_id: user.id, kind: "verify", delivery: "email", link: "javascript:alert(1)" },
    ];
    for (const body of bad) {
      const r = await call(h, "POST", `${base()}/codes`, { body });
      expect(r.status, JSON.stringify(body)).toBe(400);
    }
    expect((await call(h, "POST", `${base()}/codes`, { body: { user_id: "nobody", kind: "verify", delivery: "return" } })).status).toBe(404);
    for (const code of ["12345", "1234567", "abcdef", 123456]) {
      expect((await verify(user.id, "verify", code as string)).status).toBe(400);
    }
    // An unknown user fails like a wrong code.
    expect((await verify("nobody", "verify", "123456")).body.error).toBe("invalid_code");
  });
});
