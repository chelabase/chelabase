// Live checks for `just data-e2e` (scripts/data-e2e.sh), run with Node 24 (it strips the
// SDK's TypeScript types). The SDK is used in its default number mode: notes.id is an int8,
// and these ids are small, so they read as numbers.
//   node scripts/e2e.mjs queries   the secret key inserts, the public key reads, a batch rolls back
//   node scripts/e2e.mjs rls       a row the policy hides is absent for the public key only
// Env: GATEWAY_URL, PUBLIC_KEY, SECRET_KEY.
import assert from "node:assert/strict";
import { createClient, DataApiError } from "../src/index.ts";

const url = process.env.GATEWAY_URL;
const pub = createClient({ url, key: process.env.PUBLIC_KEY });
const sec = createClient({ url, key: process.env.SECRET_KEY });

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

const phase = phases[process.argv[2]];
assert.ok(phase, `usage: e2e.mjs ${Object.keys(phases).join("|")}`);
await phase();
