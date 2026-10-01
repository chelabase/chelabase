import { describe, expect, test } from "vitest";
import { createClient, DataApiError } from "./index.ts";
import { type Database, types } from "./test-db.ts";
import { fakeFetch } from "./test-fetch.ts";

const opts = { url: "http://localhost:8000", key: "chk_pub_x" };

describe("errors", () => {
  test("a non-2xx error body becomes a DataApiError with every field", async () => {
    const body = JSON.stringify({
      error: {
        code: "conflict",
        message: "duplicate key value violates unique constraint",
        detail: "Key (id)=(1) already exists.",
        hint: "use another id",
        sqlstate: "23505",
      },
    });
    const db = createClient<Database>({ types, ...opts, fetch: fakeFetch(body, 409) });
    const err = await db
      .from("orders")
      .insert([{ total: "1", region: "eu" }])
      .then(
        () => undefined,
        (e: unknown) => e,
      );
    expect(err).toBeInstanceOf(DataApiError);
    expect(err).toBeInstanceOf(Error);
    expect(err).toMatchObject({
      name: "DataApiError",
      status: 409,
      code: "conflict",
      message: "duplicate key value violates unique constraint",
      detail: "Key (id)=(1) already exists.",
      hint: "use another id",
      sqlstate: "23505",
    });
  });

  test("optional fields are absent when the body leaves them out", async () => {
    const body = JSON.stringify({ error: { code: "unauthorized", message: "invalid key" } });
    const db = createClient<Database>({ types, ...opts, fetch: fakeFetch(body, 401) });
    const err = (await db.from("orders").then(undefined, (e: unknown) => e)) as DataApiError;
    expect(err.status).toBe(401);
    expect(err.code).toBe("unauthorized");
    expect(err.detail).toBeUndefined();
    expect(err.hint).toBeUndefined();
    expect(err.sqlstate).toBeUndefined();
  });

  test.each([
    [401, "unauthorized"],
    [413, "too_large"],
    [429, "rate_limited"],
    [502, "unavailable"],
    [503, "unavailable"],
    [504, "timeout"],
    [500, "internal"],
    [404, "internal"],
    [418, "internal"],
  ])("a %i without a data API error body maps to %s", async (status, code) => {
    const page = `<html>proxy says ${status}</html>${"x".repeat(300)}`;
    const db = createClient<Database>({
      types,
      ...opts,
      fetch: fakeFetch(page, status, { "content-type": "text/html" }),
    });
    const err = (await db.call("add_points", { user_id: "u" }).then(
      () => undefined,
      (e: unknown) => e,
    )) as DataApiError;
    expect(err).toBeInstanceOf(DataApiError);
    expect(err.status).toBe(status);
    expect(err.code).toBe(code);
    expect(err.message).toContain(String(status));
    expect(err.message).toContain(`proxy says ${status}`);
    expect(err.message).not.toContain("x".repeat(250));
  });

  test("a 2xx answer that isn't JSON becomes a DataApiError, not a SyntaxError", async () => {
    const db = createClient<Database>({
      types,
      ...opts,
      fetch: fakeFetch("<html>hello</html>", 200, { "content-type": "text/html" }),
    });
    const err = (await db.from("orders").then(undefined, (e: unknown) => e)) as DataApiError;
    expect(err).toBeInstanceOf(DataApiError);
    expect(err.status).toBe(200);
    expect(err.code).toBe("internal");
    expect(err.message).toContain("<html>hello</html>");
  });

  test("a network error is rethrown unchanged", async () => {
    const boom = new TypeError("fetch failed");
    const fetch = (async () => {
      throw boom;
    }) as typeof globalThis.fetch;
    const db = createClient<Database>({ types, ...opts, fetch });
    await expect(db.from("orders").then()).rejects.toBe(boom);
  });
});
