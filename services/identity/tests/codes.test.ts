// Email codes: issue, deliver and consume verify, reset, invite and email_change codes.
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
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

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
    // Invites from the orchestrator are members-pool only.
    await createPool(h, "members");
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
  const lookup = (token: string, p = pool) => call(h, "POST", `${base(p)}/sessions/lookup`, { body: { token } });

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

    // Past the replacement window, so the emailed code replaces the returned one.
    vi.useFakeTimers({ toFake: ["Date"] });
    vi.setSystemTime(Date.now() + 61_000);
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
    const c = await createUser();
    const inviteC = await issue(c.id, "invite");
    expect(Date.parse(reset.expires_at)).toBe(t0 + 15 * MINUTE);
    expect(Date.parse(verifyCode.expires_at)).toBe(t0 + 15 * MINUTE);
    expect(Date.parse(inviteA.expires_at)).toBe(t0 + 72 * HOUR);

    vi.setSystemTime(t0 + 15 * MINUTE + 1000);
    expect((await verify(a.id, "reset", reset.code, { new_password: "password-ttl-1" })).status).toBe(400);
    expect((await verify(a.id, "verify", verifyCode.code)).status).toBe(400);
    expect((await verify(a.id, "invite", inviteA.code, { new_password: "password-ttl-1" })).status).toBe(200);

    // Still good just before 72 hours, refused just after.
    vi.setSystemTime(t0 + 72 * HOUR - 1000);
    expect((await verify(c.id, "invite", inviteC.code, { new_password: "password-ttl-3" })).status).toBe(200);
    vi.setSystemTime(t0 + 72 * HOUR + 1000);
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

  it("a verify code dies across an email round trip", async () => {
    const user = await createUser();
    const { code } = await issue(user.id, "verify");
    const setEmail = async (email: string) => {
      const r = await call(h, "PUT", `${base()}/users/${user.id}/email`, { body: { email } });
      expect(r.status, JSON.stringify(r.body)).toBe(200);
    };
    await setEmail(`away-${++n}-${Date.now()}@y.io`);
    await setEmail(user.email);
    const r = await verify(user.id, "verify", code);
    expect(r.status, JSON.stringify(r.body)).toBe(400);
    expect(r.body.error).toBe("invalid_code");
    expect((await call(h, "GET", `${base()}/users/${user.id}`)).body.user).toMatchObject({ email: user.email, email_verified: false });
    // Setting the same address again changes nothing, so it revokes nothing.
    const fresh = await issue(user.id, "verify");
    await setEmail(user.email.toUpperCase());
    expect((await verify(user.id, "verify", fresh.code)).status).toBe(200);
  });

  // S1: an invite never overwrites an established account (a password and a verified email).
  it("invite_refused_for_user_with_password", async () => {
    const m = "members";
    const user = await createUser("password-est-0", m);
    expect((await call(h, "POST", `${base(m)}/users/${user.id}/verify-email`)).status).toBe(200);
    for (const as of ["auth", "orchestrator"] as const) {
      const r = await call(h, "POST", `${base(m)}/codes`, { as, body: { user_id: user.id, kind: "invite", delivery: "return" } });
      expect(r.status, JSON.stringify(r.body)).toBe(409);
      expect(r.body.error).toBe("invite_refused");
      expect(r.body.code).toBeUndefined();
    }
    // Other kinds are unaffected.
    expect((await call(h, "POST", `${base(m)}/codes`, { body: { user_id: user.id, kind: "reset", delivery: "return" } })).status).toBe(201);
    expect((await signIn(user.email, "password-est-0", m)).status).toBe(201);
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
    const m = "members";
    const user = await createUser("password-pre-0", m);
    const s = await signIn(user.email, "password-pre-0", m);
    expect(s.status).toBe(201);
    const { code } = await issue(user.id, "invite", m);
    const orch = await call(h, "POST", `${base(m)}/codes`, { as: "orchestrator", body: { user_id: user.id, kind: "invite", delivery: "return" } });
    expect(orch.status).toBe(201);
    // The orchestrator's code replaced the first.
    expect((await verify(user.id, "invite", code, { new_password: "password-pre-1" }, m)).status).toBe(400);
    const r = await verify(user.id, "invite", orch.body.code, { new_password: "password-pre-1" }, m);
    expect(r.status).toBe(200);
    expect(r.body.user.email_verified).toBe(true);
    expect((await lookup(s.body.session_token, m)).status).toBe(404);
    expect((await signIn(user.email, "password-pre-0", m)).status).toBe(401);
    expect((await signIn(user.email, "password-pre-1", m)).status).toBe(201);
  });

  it("caller_allow_list", async () => {
    const m = "members";
    const user = await createUser(undefined, m);
    const create = (as: "auth" | "orchestrator" | "worker", kind: string, p = m) =>
      call(h, "POST", `${base(p)}/codes`, { as, body: { user_id: user.id, kind, delivery: "return" } });
    expect((await create("orchestrator", "invite")).status).toBe(201);
    expect((await create("orchestrator", "reset")).status).toBe(403);
    expect((await create("orchestrator", "verify")).status).toBe(403);
    // App users have no invites: the orchestrator's codes are members-pool only.
    expect((await create("orchestrator", "invite", pool)).status).toBe(403);
    expect((await create("worker", "invite")).status).toBe(403);
    expect((await call(h, "POST", `${base(m)}/codes/verify`, { as: "orchestrator", body: {} })).status).toBe(403);
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

  // The stored failure budget (10 wrong guesses per (user, kind) per 24 h, across codes) and the
  // replacement window (a live code younger than 60 s stands, except invites).
  const schemaOf = (p: string) => `identity_app_${p.slice("app-".length).replaceAll("-", "")}`;
  const budgetRow = async (userId: string, kind: string) =>
    (
      await h.db.query(
        `SELECT failures, "failuresSince" AS since, "consumedAt" AS consumed FROM "${schemaOf(pool)}".email_code
          WHERE "userId" = $1 AND kind = $2`,
        [userId, kind],
      )
    ).rows[0] as { failures: number | null; since: Date | null; consumed: Date | null } | undefined;
  /** `n` wrong guesses at `code`, each refused as a wrong code. */
  const guessWrong = async (userId: string, kind: string, code: string, n: number, extra: Record<string, unknown> = {}) => {
    for (let i = 1; i <= n; i++) {
      const r = await verify(userId, kind, wrong(code, i), extra);
      expect(r.status, JSON.stringify(r.body)).toBe(400);
      expect(r.body.error).toBe("invalid_code");
    }
  };
  const fakeNow = () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const t0 = Date.now();
    vi.setSystemTime(t0);
    return t0;
  };

  it("the failure budget survives new codes and stops guesses with the right code", async () => {
    const t0 = fakeNow();
    const pw = { new_password: "password-budget-2" };
    const user = await createUser("password-budget-1");
    // 5 + 5 wrong guesses over two codes; each is locked, so the next one replaces it at once.
    await guessWrong(user.id, "reset", (await issue(user.id, "reset")).code, 5, pw);
    vi.setSystemTime(t0 + MINUTE);
    await guessWrong(user.id, "reset", (await issue(user.id, "reset")).code, 5, pw);
    // A new code doesn't refill the budget: the right code is refused, unchecked.
    vi.setSystemTime(t0 + 2 * MINUTE);
    const fresh = await issue(user.id, "reset");
    const r = await verify(user.id, "reset", fresh.code, pw);
    expect(r.status, JSON.stringify(r.body)).toBe(429);
    expect(r.body.error).toBe("code_budget_spent");
    expect((await signIn(user.email, "password-budget-2")).status).toBe(401);
    expect((await budgetRow(user.id, "reset"))!.consumed).toBeNull();
    // Still spent just before the window ends; a code issued then works once it has.
    vi.setSystemTime(t0 + DAY - MINUTE);
    const late = await issue(user.id, "reset");
    expect((await verify(user.id, "reset", late.code, pw)).body.error).toBe("code_budget_spent");
    vi.setSystemTime(t0 + DAY + 1000);
    expect((await verify(user.id, "reset", late.code, pw)).status).toBe(200);
    expect((await signIn(user.email, "password-budget-2")).status).toBe(201);
  });

  it("failure budgets are per kind", async () => {
    const t0 = fakeNow();
    const user = await createUser();
    const pw = { new_password: "password-kind-1" };
    await guessWrong(user.id, "reset", (await issue(user.id, "reset")).code, 5, pw);
    vi.setSystemTime(t0 + MINUTE);
    const reset = await issue(user.id, "reset");
    await guessWrong(user.id, "reset", reset.code, 5, pw);
    vi.setSystemTime(t0 + 2 * MINUTE);
    const spent = await issue(user.id, "reset");
    expect((await verify(user.id, "reset", spent.code, pw)).body.error).toBe("code_budget_spent");
    // Other kinds for the same user are unaffected.
    expect((await verify(user.id, "verify", (await issue(user.id, "verify")).code)).status).toBe(200);
    expect((await verify(user.id, "invite", (await issue(user.id, "invite")).code, pw)).status).toBe(200);
    expect((await verify(user.id, "reset", spent.code, pw)).body.error).toBe("code_budget_spent");
  });

  it("a right code clears the failure budget", async () => {
    const t0 = fakeNow();
    const user = await createUser();
    const pw = { new_password: "password-clear-1" };
    await guessWrong(user.id, "reset", (await issue(user.id, "reset")).code, 5, pw);
    vi.setSystemTime(t0 + MINUTE);
    const second = await issue(user.id, "reset");
    await guessWrong(user.id, "reset", second.code, 4, pw);
    expect(await budgetRow(user.id, "reset")).toMatchObject({ failures: 9 });
    expect((await verify(user.id, "reset", second.code, pw)).status).toBe(200);
    const row = (await budgetRow(user.id, "reset"))!;
    expect(row.failures).toBe(0);
    expect(row.since).toBeNull();
    expect(row.consumed).not.toBeNull();
    // A full budget again: 9 more wrong guesses and the right code still work.
    await guessWrong(user.id, "reset", (await issue(user.id, "reset")).code, 5, pw);
    const third = await issue(user.id, "reset");
    await guessWrong(user.id, "reset", third.code, 4, pw);
    expect((await verify(user.id, "reset", third.code, pw)).status).toBe(200);
  });

  it("an unknown user costs no budget", async () => {
    const unknown = "0b9e6f4c-3c1e-4d8a-9f6b-2a7d5e8c1f03";
    for (let i = 0; i < 12; i++) {
      const r = await verify(unknown, "reset", "123456", { new_password: "password-unknown-1" });
      expect(r.status).toBe(400);
      expect(r.body.error).toBe("invalid_code");
    }
    expect(await budgetRow(unknown, "reset")).toBeUndefined();
  });

  it("a live code is not replaced within the window", async () => {
    const t0 = fakeNow();
    const p = newAppPool().pool;
    await createPool(mailed, p);
    const created = await call(mailed, "POST", `${base(p)}/users`, { body: { email: `window-${Date.now()}@x.io` } });
    const user = created.body.user as { id: string };
    const ask = (kind = "reset", delivery = "email") =>
      call(mailed, "POST", `${base(p)}/codes`, { body: { user_id: user.id, kind, delivery } });
    const pw = { new_password: "password-window-1" };
    const first = await ask("reset", "return");
    expect(first.status).toBe(201);
    vi.setSystemTime(t0 + 30_000);
    const before = sink.messages.length;
    const refused = await ask();
    expect(refused.status, JSON.stringify(refused.body)).toBe(429);
    expect(refused.body.error).toBe("code_recently_sent");
    expect(sink.messages.length).toBe(before);
    // The earlier code still works.
    expect((await verify(user.id, "reset", first.body.code, pw, p, mailed)).status).toBe(200);
    // After 60 s a live code is replaced.
    const v1 = await ask("verify", "return");
    expect(v1.status).toBe(201);
    vi.setSystemTime(t0 + 30_000 + 59_000);
    expect((await ask("verify", "return")).status).toBe(429);
    vi.setSystemTime(t0 + 30_000 + 61_000);
    const v2 = await ask("verify", "return");
    expect(v2.status).toBe(201);
    if (v2.body.code !== v1.body.code) {
      expect((await verify(user.id, "verify", v1.body.code, {}, p, mailed)).status).toBe(400);
    }
    expect((await verify(user.id, "verify", v2.body.code, {}, p, mailed)).status).toBe(200);
  });

  it("a refused send leaves no code standing, so an immediate retry issues and mails one", async () => {
    const p = newAppPool().pool;
    await createPool(mailed, p);
    const email = `refused-${Date.now()}@x.io`;
    const created = await call(mailed, "POST", `${base(p)}/users`, { body: { email } });
    const user = created.body.user as { id: string };
    const ask = () => call(mailed, "POST", `${base(p)}/codes`, { body: { user_id: user.id, kind: "verify", delivery: "email" } });
    vi.spyOn(console, "error").mockImplementation(() => undefined);
    sink.refuse.add(email);
    try {
      const r = await ask();
      expect(r.status, JSON.stringify(r.body)).toBe(502);
      expect(r.body.error).toBe("email_send_failed");
    } finally {
      sink.refuse.delete(email);
    }
    const before = sink.messages.length;
    const again = await ask();
    expect(again.status, JSON.stringify(again.body)).toBe(201);
    expect(sink.messages.length).toBe(before + 1);
    const mailedCode = /\b([0-9]{6})\b/.exec(sink.messages.at(-1)!.data)?.[1];
    expect(mailedCode).toMatch(/^[0-9]{6}$/);
    expect((await verify(user.id, "verify", mailedCode!, {}, p, mailed)).status).toBe(200);
  });

  it("the failure budget survives an email change (users.set_email)", async () => {
    const user = await createUser("password-moved-1");
    const pw = { new_password: "password-moved-2" };
    // 5 + 5 wrong guesses over two codes; each is locked, so the next one replaces it at once.
    for (let c = 0; c < 2; c++) await guessWrong(user.id, "reset", (await issue(user.id, "reset")).code, 5, pw);
    const moved = await call(h, "PUT", `${base()}/users/${user.id}/email`, { body: { email: `moved-${Date.now()}@x.io` } });
    expect(moved.status, JSON.stringify(moved.body)).toBe(200);
    const fresh = await issue(user.id, "reset");
    const r = await verify(user.id, "reset", fresh.code, pw);
    expect(r.status, JSON.stringify(r.body)).toBe(429);
    expect(r.body.error).toBe("code_budget_spent");
  });

  it("a consumed or locked code can be replaced at once", async () => {
    fakeNow();
    const user = await createUser();
    // Consumed.
    expect((await verify(user.id, "verify", (await issue(user.id, "verify")).code)).status).toBe(200);
    const v = await issue(user.id, "verify");
    // Locked by 5 wrong guesses.
    const pw = { new_password: "password-locked-1" };
    await guessWrong(user.id, "reset", (await issue(user.id, "reset")).code, 5, pw);
    const next = await issue(user.id, "reset");
    expect((await verify(user.id, "reset", next.code, pw)).status).toBe(200);
    // Expired (15 minutes, so past the window too).
    vi.setSystemTime(Date.now() + 16 * MINUTE);
    expect((await verify(user.id, "verify", v.code)).status).toBe(400);
    await issue(user.id, "verify");
  });

  it("invite codes are replaced at once", async () => {
    const user = await createUser();
    const a = await issue(user.id, "invite");
    const b = await issue(user.id, "invite");
    const pw = { new_password: "password-invite-again-1" };
    if (a.code !== b.code) expect((await verify(user.id, "invite", a.code, pw)).status).toBe(400);
    expect((await verify(user.id, "invite", b.code, pw)).status).toBe(200);
  });

  // email_change: mailed to the requested new address only, bound to the current and the new
  // address, and nothing changes until it is confirmed. These run on the SMTP harness.
  describeDb("email_change", () => {
    let p: string;
    beforeAll(async () => {
      p = newAppPool().pool;
      await createPool(mailed, p);
    });
    const at = (tag: string) => `${tag}-${++n}-${Date.now()}@x.io`;
    const mkUser = async (password?: string, email = at("ec")) => {
      const r = await call(mailed, "POST", `${base(p)}/users`, {
        body: { email, email_verified: true, ...(password ? { password } : {}) },
      });
      expect(r.status, JSON.stringify(r.body)).toBe(201);
      return r.body.user as { id: string; email: string; email_verified: boolean };
    };
    const getUser = async (id: string) => (await call(mailed, "GET", `${base(p)}/users/${id}`)).body.user;
    const request = (userId: string, newEmail: string, extra: Record<string, unknown> = {}, as: "auth" | "orchestrator" = "auth") =>
      call(mailed, "POST", `${base(p)}/codes`, {
        as,
        body: { user_id: userId, kind: "email_change", delivery: "email", new_email: newEmail, project_name: "Acme", ...extra },
      });
    const textPart = (data: string) => data.split(/Content-Type: text\/plain/i)[1]!;
    /** Requests a code for `newEmail` and reads it from the one message it sent. */
    const issueChange = async (userId: string, newEmail: string) => {
      const before = sink.messages.length;
      const r = await request(userId, newEmail);
      expect(r.status, JSON.stringify(r.body)).toBe(201);
      expect(r.body.code).toBeUndefined();
      expect(sink.messages.length).toBe(before + 1);
      const message = sink.messages.at(-1)!;
      const code = /address is:\s+([0-9]{6})\b/.exec(textPart(message.data))![1]!;
      return { reply: r, message, code };
    };
    const confirm = (userId: string, newEmail: string, code: string, extra: Record<string, unknown> = {}) =>
      verify(userId, "email_change", code, { new_email: newEmail, ...extra }, p, mailed);

    it("email_change code is mailed to the new address and changes nothing", async () => {
      const user = await mkUser();
      const next = at("new");
      const { reply, message, code } = await issueChange(user.id, next);
      expect(reply.body.kind).toBe("email_change");
      expect(message.to).toEqual([next]);
      expect(message.data).toContain("Acme");
      expect(JSON.stringify(reply.body)).not.toContain(code);
      expect(await getUser(user.id)).toMatchObject({ email: user.email, email_verified: true });
      // Mixed case is the same address (asked again past the replacement window).
      vi.useFakeTimers({ toFake: ["Date"] });
      vi.setSystemTime(Date.now() + 61_000);
      const again = await request(user.id, next.toUpperCase());
      expect(again.status).toBe(201);
      expect(sink.messages.at(-1)!.to).toEqual([next]);
    });

    it("email_change confirmation swaps, verifies and notifies the old address", async () => {
      const unverified = await call(mailed, "POST", `${base(p)}/users`, { body: { email: at("old") } });
      const user = unverified.body.user as { id: string; email: string };
      const next = at("new");
      const { code } = await issueChange(user.id, next);
      const before = sink.messages.length;
      const r = await confirm(user.id, next, code);
      expect(r.status, JSON.stringify(r.body)).toBe(200);
      expect(r.body.user).toMatchObject({ id: user.id, email: next, email_verified: true });
      expect(await getUser(user.id)).toMatchObject({ email: next, email_verified: true });
      expect(sink.messages.length).toBe(before + 1);
      const notice = sink.messages.at(-1)!;
      expect(notice.to).toEqual([user.email]);
      expect(notice.data).toContain("was changed");
      // Neutral advice (A4): ask the app's support, not a password reset.
      expect(notice.data).toContain("contact");
      expect(notice.data).not.toMatch(/reset your password/i);
      expect(notice.data).not.toContain(next);
      expect(notice.data).not.toContain(code);
      // Single use.
      expect((await confirm(user.id, next, code)).body.error).toBe("invalid_code");
    });

    it("email_change notice under a name identity cannot show uses the default", async () => {
      for (const bad of ["\ufeff", "\ufeff ", "Acme\nInc"]) {
        const user = await mkUser();
        const next = at("new");
        const issued = await request(user.id, next, { project_name: bad });
        expect(issued.status, JSON.stringify(bad) + JSON.stringify(issued.body)).toBe(201);
        const mail = sink.messages.at(-1)!;
        expect(mail.to).toEqual([next]);
        expect(mail.data).not.toContain("Acme");
        const code = /address is:\s+([0-9]{6})\b/.exec(textPart(mail.data))![1]!;
        const before = sink.messages.length;
        const r = await confirm(user.id, next, code, { project_name: bad });
        expect(r.status, JSON.stringify(bad) + JSON.stringify(r.body)).toBe(200);
        expect(sink.messages.length).toBe(before + 1);
        expect(sink.messages.at(-1)!.data).toContain("Your Chelabase email address was changed");
      }
    });

    it("email_change notice names the project", async () => {
      const user = await mkUser();
      const next = at("new");
      const { code } = await issueChange(user.id, next);
      const before = sink.messages.length;
      const r = await confirm(user.id, next, code, { project_name: "Acme" });
      expect(r.status, JSON.stringify(r.body)).toBe(200);
      expect(sink.messages.length).toBe(before + 1);
      const notice = sink.messages.at(-1)!;
      expect(notice.to).toEqual([user.email]);
      expect(notice.data).toContain("Your Acme email address was changed");
    });

    it("email_change code is bound to the requested address", async () => {
      const user = await mkUser();
      const next = at("new");
      const { code } = await issueChange(user.id, next);
      const other = at("other");
      for (let k = 1; k <= 5; k++) {
        // Alternately the right code for another address, and a wrong code for the right one.
        const r = k % 2 === 1 ? await confirm(user.id, other, code) : await confirm(user.id, next, wrong(code, k));
        expect(r.status, JSON.stringify(r.body)).toBe(400);
        expect(r.body.error).toBe("invalid_code");
        expect((await getUser(user.id)).email).toBe(user.email);
      }
      const right = await confirm(user.id, next, code);
      expect(right.status).toBe(400);
      expect(right.body.error).toBe("invalid_code");
      expect((await getUser(user.id)).email).toBe(user.email);
      // Refused without new_email, and with new_password, before a guess is counted.
      const fresh = await issueChange(user.id, next);
      expect((await verify(user.id, "email_change", fresh.code, {}, p, mailed)).status).toBe(400);
      expect((await confirm(user.id, next, fresh.code, { new_password: "password-ec-1" })).body.error).toBe("invalid_request");
      expect((await confirm(user.id, next, fresh.code)).status).toBe(200);
    });

    it("email_change code dies when the current email changes", async () => {
      const user = await mkUser();
      const next = at("new");
      const { code } = await issueChange(user.id, next);
      const moved = await call(mailed, "PUT", `${base(p)}/users/${user.id}/email`, { body: { email: at("moved"), email_verified: true } });
      expect(moved.status).toBe(200);
      const r = await confirm(user.id, next, code);
      expect(r.status).toBe(400);
      expect(r.body.error).toBe("invalid_code");
      expect((await getUser(user.id)).email).toBe(moved.body.user.email);

      // A round trip A -> C -> A revokes the code: it does not come back with the address.
      const second = await issueChange(user.id, next);
      const setEmail = async (email: string) => {
        const r = await call(mailed, "PUT", `${base(p)}/users/${user.id}/email`, { body: { email, email_verified: true } });
        expect(r.status, JSON.stringify(r.body)).toBe(200);
      };
      const home = moved.body.user.email as string;
      await setEmail(at("away"));
      await setEmail(home);
      const back = await confirm(user.id, next, second.code);
      expect(back.status, JSON.stringify(back.body)).toBe(400);
      expect(back.body.error).toBe("invalid_code");
      expect(await getUser(user.id)).toMatchObject({ email: home });

      // The swap revokes too: a round trip made of two swaps (A -> B -> A) leaves a code bound
      // to A dead.
      const reset = await call(mailed, "POST", `${base(p)}/codes`, { body: { user_id: user.id, kind: "reset", delivery: "return" } });
      expect(reset.status, JSON.stringify(reset.body)).toBe(201);
      const there = await issueChange(user.id, next);
      expect((await confirm(user.id, next, there.code)).status).toBe(200);
      const andBack = await issueChange(user.id, home);
      expect((await confirm(user.id, home, andBack.code)).status).toBe(200);
      expect(await getUser(user.id)).toMatchObject({ email: home });
      const stale = await verify(user.id, "reset", reset.body.code, { new_password: "password-ec-rt-1" }, p, mailed);
      expect(stale.status, JSON.stringify(stale.body)).toBe(400);
      expect(stale.body.error).toBe("invalid_code");
    });

    it("email_change confirmation succeeds when the notice is refused", async () => {
      const user = await mkUser();
      const next = at("new");
      const { code } = await issueChange(user.id, next);
      const logged: string[] = [];
      vi.spyOn(console, "error").mockImplementation((...args: unknown[]) => void logged.push(args.map(String).join(" ")));
      sink.refuse.add(user.email.toLowerCase());
      try {
        const before = sink.messages.length;
        const r = await confirm(user.id, next, code);
        expect(r.status, JSON.stringify(r.body)).toBe(200);
        expect(r.body.user).toMatchObject({ id: user.id, email: next, email_verified: true });
        expect(await getUser(user.id)).toMatchObject({ email: next, email_verified: true });
        expect(sink.messages.length).toBe(before);
      } finally {
        sink.refuse.delete(user.email.toLowerCase());
      }
      expect(logged.some((l) => l.includes("email change notice was not sent"))).toBe(true);
      for (const l of logged) {
        expect(l).not.toContain(code);
        expect(l).not.toContain(next);
      }
    });

    it("email_change confirmation re-checks the address is free", async () => {
      const user = await mkUser();
      const next = at("new");
      const { code } = await issueChange(user.id, next);
      await mkUser(undefined, next);
      const r = await confirm(user.id, next, code);
      expect(r.status, JSON.stringify(r.body)).toBe(409);
      expect(r.body.error).toBe("conflict");
      expect(await getUser(user.id)).toMatchObject({ email: user.email, email_verified: true });
      // The code is spent.
      expect((await confirm(user.id, next, code)).body.error).toBe("invalid_code");
    });

    it("email_change refuses a taken or unchanged address and return delivery", async () => {
      const user = await mkUser();
      const taken = (await mkUser()).email;
      const before = sink.messages.length;
      for (const addr of [taken, user.email, user.email.toUpperCase()]) {
        const r = await request(user.id, addr);
        expect(r.status, addr).toBe(409);
        expect(r.body.error).toBe("conflict");
        expect(r.body.code).toBeUndefined();
      }
      const ret = await request(user.id, at("new"), { delivery: "return" });
      expect(ret.status).toBe(400);
      expect(ret.body.code).toBeUndefined();
      for (const bad of [undefined, "", "not-an-email", `${"a".repeat(320)}@x.io`]) {
        const body: Record<string, unknown> = { user_id: user.id, kind: "email_change", delivery: "email" };
        if (bad !== undefined) body.new_email = bad;
        expect((await call(mailed, "POST", `${base(p)}/codes`, { body })).status, String(bad)).toBe(400);
      }
      // new_email belongs to email_change only.
      const stray = await call(mailed, "POST", `${base(p)}/codes`, {
        body: { user_id: user.id, kind: "verify", delivery: "return", new_email: at("new") },
      });
      expect(stray.status).toBe(400);
      expect(sink.messages.length).toBe(before);
      // Nothing was issued: no code is pending for the user.
      expect((await confirm(user.id, taken, "000000")).body.error).toBe("invalid_code");
    });

    it("only auth may create email_change codes", async () => {
      const user = await mkUser();
      const m = "members";
      const member = await call(mailed, "POST", `${base(m)}/users`, { body: { email: at("member") } });
      for (const [as, target, id] of [
        ["orchestrator", m, member.body.user.id],
        ["orchestrator", p, user.id],
        ["worker", p, user.id],
      ] as const) {
        const r = await call(mailed, "POST", `${base(target)}/codes`, {
          as,
          body: { user_id: id, kind: "email_change", delivery: "email", new_email: at("new") },
        });
        expect(r.status, `${as} ${target}`).toBe(403);
      }
    });

    it("email_change codes never stand in for another kind, nor another kind for them", async () => {
      // No password yet, so an invite code may be issued for the user too.
      const user = await mkUser();
      const next = at("new");
      const { code } = await issueChange(user.id, next);
      expect((await verify(user.id, "verify", code, {}, p, mailed)).body.error).toBe("invalid_code");
      for (const kind of ["reset", "invite"]) {
        expect((await verify(user.id, kind, code, { new_password: "password-kind-1" }, p, mailed)).body.error).toBe("invalid_code");
      }
      // Codes of the other kinds, presented as email_change.
      for (const kind of ["verify", "reset", "invite"]) {
        const other = await call(mailed, "POST", `${base(p)}/codes`, { body: { user_id: user.id, kind, delivery: "return" } });
        expect(other.status, kind).toBe(201);
        const r = await confirm(user.id, next, other.body.code);
        expect(r.status, kind).toBe(400);
        expect(r.body.error).toBe("invalid_code");
      }
      expect((await getUser(user.id)).email).toBe(user.email);
      // The email_change code was never spent by any of that.
      expect((await confirm(user.id, next, code)).status).toBe(200);
    });
  });
});
