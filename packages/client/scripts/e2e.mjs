// Live checks for `just data-e2e` (scripts/data-e2e.sh), run with Node 24 (it strips the
// SDK's TypeScript types). The SDK is used in its default number mode: notes.id is an int8,
// and these ids are small, so they read as numbers.
//   node scripts/e2e.mjs queries   the secret key inserts, the public key reads, a batch rolls back
//   node scripts/e2e.mjs rls       a row the policy hides is absent for the public key only
// Env: GATEWAY_URL, PUBLIC_KEY, SECRET_KEY.
//
// App users, for `just app-users-e2e` (scripts/app-users-e2e.sh); each user's refresh token is
// kept in a file under STATE_DIR between phases:
//   node scripts/e2e.mjs auth-signup-refused   a NOT NULL app column without data: 422
//   node scripts/e2e.mjs auth-signup           A and B sign up with data (STATE_DIR/users.json)
//   node scripts/e2e.mjs auth-rls              own-row policies on users and a developer table
//   node scripts/e2e.mjs auth-refresh          rotation, grace, reuse and the revoked session
// Env: GATEWAY_URL, PUBLIC_KEY, STATE_DIR, STAMP (makes the emails unique).
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { createClient, DataApiError } from "../src/index.ts";

const url = process.env.GATEWAY_URL;
const pub = createClient({ url, key: process.env.PUBLIC_KEY });
const sec = createClient({ url, key: process.env.SECRET_KEY ?? "unused" });
const PASSWORD = "Sup3r-Secret-pw!";

/** A refresh-token store in one JSON file per user, so a session outlives the process. */
function fileStorage(name) {
  const path = `${process.env.STATE_DIR}/session-${name}.json`;
  const read = () => {
    try {
      return JSON.parse(readFileSync(path, "utf8"));
    } catch {
      return {};
    }
  };
  return {
    get: (k) => read()[k] ?? null,
    set: (k, v) => writeFileSync(path, JSON.stringify({ ...read(), [k]: v })),
    remove: (k) => {
      const m = read();
      delete m[k];
      writeFileSync(path, JSON.stringify(m));
    },
  };
}

/** The app users client of `name` (a, b): the public key plus that user's stored session. */
function userClient(name) {
  const storage = fileStorage(name);
  return { db: createClient({ url, key: process.env.PUBLIC_KEY, auth: { storage, autoRefresh: false } }), storage };
}

function email(name) {
  return `${name}-${process.env.STAMP}@example.com`;
}

/** A raw call through the gateway: `[status, body]`. */
async function raw(path, { method = "POST", body, user } = {}) {
  const headers = { authorization: `Bearer ${process.env.PUBLIC_KEY}`, "content-type": "application/json" };
  if (user !== undefined) headers["chelabase-user"] = user;
  const res = await fetch(`${url}${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {}
  return [res.status, json ?? text];
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function refused(promise, status, code) {
  try {
    await promise;
  } catch (e) {
    assert.ok(e instanceof DataApiError, `expected a DataApiError, got ${e}`);
    assert.equal(e.status, status, `${e.code}: ${e.message}`);
    assert.equal(e.code, code, e.message);
    return e;
  }
  assert.fail(`the call succeeded; wanted ${status} ${code}`);
}

const phases = {
  async queries() {
    const made = await sec
      .from("notes")
      .insert([{ body: "first" }, { body: "second" }])
      .returning("id", "body");
    assert.deepEqual(made.map((r) => r.body), ["first", "second"]);
    assert.ok(made.every((r) => typeof r.id === "number"), JSON.stringify(made));
    console.log("secret key inserted 2 rows");

    const seen = await pub.from("notes").select("id", "body").orderBy("id");
    assert.deepEqual(seen, made.map(({ id, body }) => ({ id, body })));
    console.log("public key selected them");

    await refused(pub.from("notes").insert({ body: "nope" }), 403, "permission_denied");
    console.log("public key insert: 403 permission_denied");

    // body is NOT NULL: the second operation fails and the first must roll back.
    const err = await sec
      .batch((tx) => [
        tx.from("notes").insert({ body: "rolled back" }),
        tx.from("notes").insert({ body: null }),
      ])
      .then(
        () => assert.fail("the batch succeeded"),
        (e) => e,
      );
    assert.ok(err instanceof DataApiError, String(err));
    assert.ok(err.status >= 400 && err.status < 500, `${err.status} ${err.code}`);
    const after = await sec.from("notes").select("body").where({ body: "rolled back" });
    assert.equal(after.length, 0, "the batch left a row behind");
    const count = await sec.from("notes").select("id");
    assert.equal(count.length, 2);
    console.log(`batch rolled back on error (${err.status} ${err.code})`);
  },

  async rls() {
    await sec.from("notes").insert({ body: "hidden" });
    const secret = await sec.from("notes").select("body").where({ body: "hidden" });
    assert.equal(secret.length, 1, "the secret key's role should see the hidden row");
    const open = await pub.from("notes").select("body").where({ body: "hidden" });
    assert.equal(open.length, 0, "the public key must not see the hidden row");
    const all = await pub.from("notes").select("body").orderBy("id");
    assert.deepEqual(all.map((r) => r.body), ["first", "second"]);
    console.log("hidden row: visible to the secret key, absent for the public key");
  },
};

Object.assign(phases, {
  async "auth-signup-refused"() {
    const { db } = userClient("a");
    await refused(db.auth.signUp({ email: email("a"), password: PASSWORD }), 422, "constraint_violation");
    console.log("sign-up without display_name: 422 constraint_violation");
  },

  async "auth-signup"() {
    const users = {};
    for (const [name, display] of [["a", "Alice"], ["b", "Bob"]]) {
      const { db } = userClient(name);
      // The refused sign-up above left no Zitadel user behind, so A's email is still free.
      const r = await db.auth.signUp({ email: email(name), password: PASSWORD, data: { display_name: display } });
      assert.ok(r.session, `${name}: no session (verification is off by default)`);
      assert.equal(r.verificationRequired, false);
      assert.equal(r.user.email, email(name));
      const me = await db.auth.getUser();
      assert.equal(me.id, r.user.id);
      users[name] = { id: r.user.id, email: email(name) };
      console.log(`${name} signed up: ${r.user.id}`);
    }
    // A second sign-up with a taken email (verification off): 409 email_taken.
    await refused(userClient("x").db.auth.signUp({ email: email("a"), password: PASSWORD, data: { display_name: "X" } }), 409, "email_taken");
    writeFileSync(`${process.env.STATE_DIR}/users.json`, JSON.stringify(users));
  },

  async "auth-rls"() {
    const users = JSON.parse(readFileSync(`${process.env.STATE_DIR}/users.json`, "utf8"));
    for (const name of ["a", "b"]) {
      const { db } = userClient(name);
      const rows = await db.from("users").select("id", "display_name");
      assert.deepEqual(rows.map((r) => r.id), [users[name].id], `${name} sees only their row`);
      // The own-row update policy: an app column of the user's own row.
      await db.from("users").update({ display_name: `${name} edited` }).where({ id: users[name].id });
      // A managed column is refused by the guard.
      await refused(db.from("users").update({ email: "x@example.com" }).where({ id: users[name].id }), 403, "permission_denied");
      await db.from("notes").insert({ body: `${name}'s note` });
    }
    console.log("users: each sees and edits only their own row; the guard refuses a managed column");
    // The public key alone (anon) has no grant on the users table: it isn't in anon's schema.
    await pub.from("users").select("id").then(
      (rows) => assert.deepEqual(rows, [], "anon read users rows"),
      (e) => assert.ok(e instanceof DataApiError && [403, 404].includes(e.status), `${e.status} ${e.code}: ${e}`),
    );
    for (const name of ["a", "b"]) {
      const { db } = userClient(name);
      const notes = await db.from("notes").select("owner", "body");
      assert.deepEqual(notes, [{ owner: users[name].id, body: `${name}'s note` }]);
    }
    const anonNotes = await pub.from("notes").select("body").then(
      (rows) => rows,
      (e) => (e instanceof DataApiError && [403, 404].includes(e.status) ? [] : Promise.reject(e)),
    );
    assert.deepEqual(anonNotes, []);
    console.log("notes: USING (owner = chelabase.user_id()) shows each user their own; anon nothing");
  },

  async "auth-refresh"() {
    const { db, storage } = userClient("a");
    const key = `chelabase.${process.env.PUBLIC_KEY.slice(0, 12)}.refresh`;
    const old = storage.get(key);
    assert.ok(old, "A has no stored refresh token");
    const s = await db.auth.refresh();
    assert.notEqual(s.refresh_token, old, "the refresh token rotated");
    const query = { v: 1, op: "select", from: "users" };
    let [status, body] = await raw("/data/v1/query", { body: query, user: s.access_token });
    assert.equal(status, 200, JSON.stringify(body));
    // Within the 10 s grace, the previous token gets the same new refresh token back.
    [status, body] = await raw("/auth/v1/app/refresh", { body: { refresh_token: old } });
    assert.equal(status, 200, JSON.stringify(body));
    assert.equal(body.refresh_token, s.refresh_token, "grace answer is the same new token");
    console.log("rotated; the previous token inside the grace got the same new token");
    await sleep(11_000);
    [status, body] = await raw("/auth/v1/app/refresh", { body: { refresh_token: old } });
    assert.equal(status, 401, JSON.stringify(body));
    assert.equal(body?.error?.code, "refresh_token_reused", JSON.stringify(body));
    console.log("reuse after 11 s: 401 refresh_token_reused");
    const t0 = Date.now();
    for (;;) {
      [status, body] = await raw("/data/v1/query", { body: query, user: s.access_token });
      if (status === 401) break;
      assert.equal(status, 200, JSON.stringify(body));
      assert.ok(Date.now() - t0 < 35_000, "the revoked session's access token still works after 35 s");
      await sleep(1000);
    }
    assert.equal(body?.error?.code, "user_session_revoked", JSON.stringify(body));
    console.log(`the session's access token: 401 user_session_revoked after ${Math.round((Date.now() - t0) / 1000)} s`);
  },
});

const phase = phases[process.argv[2]];
assert.ok(phase, `usage: e2e.mjs ${Object.keys(phases).join("|")}`);
await phase();
