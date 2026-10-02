import { ApiError, type ColumnDesc } from "@cheladb/management-client";
import { describe, expect, it } from "vitest";
import { buildFilterTree, buildSelect, consoleErrorText, filterOpsFor, parseValue, whereByKey } from "./console";

const col = (over: Partial<ColumnDesc> = {}): ColumnDesc => ({
  type: "text",
  nullable: false,
  has_default: false,
  generated: false,
  array: false,
  enum: null,
  ...over,
});
const columns = {
  id: col({ type: "integer", has_default: true }),
  name: col(),
  qty: col({ type: "integer", nullable: true }),
};

describe("buildSelect", () => {
  it("pages 50 at a time, qualifies the table and sorts", () => {
    expect(buildSelect({ schema: "public", table: "items", page: 0, sort: null, filters: null, pk: [] })).toEqual({
      v: 1,
      op: "select",
      from: "public.items",
      limit: 50,
      offset: 0,
    });
    expect(
      buildSelect({ schema: "public", table: "items", page: 2, sort: { col: "name", dir: "desc" }, filters: null, pk: ["id"] }),
    ).toMatchObject({ offset: 100, limit: 50, orderBy: [{ col: "name", dir: "desc" }, { col: "id", dir: "asc" }] });
    expect(buildSelect({ schema: "p", table: "t", page: 0, sort: null, filters: null, pk: ["a", "b"] }).orderBy).toEqual([
      { col: "a", dir: "asc" },
      { col: "b", dir: "asc" },
    ]);
    expect(buildSelect({ schema: "p", table: "t", page: 0, sort: { col: "id", dir: "asc" }, filters: null, pk: ["id"] }).orderBy).toEqual([{ col: "id", dir: "asc" }]);
  });
});

describe("buildFilterTree", () => {
  it("a single filter is bare and several are combined with and/or", () => {
    const one = buildFilterTree({ mode: "and", rows: [{ col: "qty", op: "gte", value: "3" }] }, columns);
    expect(one).toEqual({ ok: true, where: { col: "qty", gte: 3 } });
    const rows = [
      { col: "qty", op: "gte" as const, value: "3" },
      { col: "name", op: "ilike" as const, value: "a%" },
    ];
    expect(buildFilterTree({ mode: "and", rows }, columns)).toEqual({
      ok: true,
      where: { and: [{ col: "qty", gte: 3 }, { col: "name", ilike: "a%" }] },
    });
    expect(buildFilterTree({ mode: "or", rows }, columns)).toMatchObject({ where: { or: expect.any(Array) } });
  });

  it("parses in lists and is values, and reports a bad value", () => {
    expect(buildFilterTree({ mode: "and", rows: [{ col: "id", op: "in", value: "1, 2" }] }, columns)).toEqual({
      ok: true,
      where: { col: "id", in: [1, 2] },
    });
    expect(buildFilterTree({ mode: "and", rows: [{ col: "qty", op: "is", value: "null" }] }, columns)).toEqual({
      ok: true,
      where: { col: "qty", is: null },
    });
    expect(buildFilterTree({ mode: "and", rows: [{ col: "qty", op: "eq", value: "abc" }] }, columns)).toMatchObject({
      ok: false,
    });
    expect(buildFilterTree({ mode: "and", rows: [] }, columns)).toEqual({ ok: true, where: undefined });
  });
});

describe("whereByKey and parseValue", () => {
  it("keys on every primary key column", () => {
    expect(whereByKey(["id"], { id: 1, name: "a" })).toEqual({ col: "id", eq: 1 });
    expect(whereByKey(["a", "b"], { a: 1, b: "x" })).toEqual({ and: [{ col: "a", eq: 1 }, { col: "b", eq: "x" }] });
  });
  it("types inputs by column type", () => {
    expect(parseValue(col({ type: "boolean" }), "true")).toEqual({ ok: true, value: true });
    expect(parseValue(col({ type: "jsonb" }), '{"a":1}')).toEqual({ ok: true, value: { a: 1 } });
    expect(parseValue(col({ type: "jsonb" }), "{oops")).toMatchObject({ ok: false });
    expect(parseValue(col({ type: "bigint" }), "12")).toEqual({ ok: true, value: 12 });
    expect(parseValue(col({ type: "integer" }), "1.5")).toMatchObject({ ok: false });
    expect(parseValue(col({ type: "text" }), "")).toEqual({ ok: true, value: "" });
    expect(parseValue(col({ type: "date" }), "2026-10-02")).toEqual({ ok: true, value: "2026-10-02" });
  });
});

describe("exact numbers", () => {
  it("sends a JSON number only when it reads back identically, else the text", () => {
    expect(parseValue(col({ type: "numeric" }), "0.1234567890123456789")).toEqual({ ok: true, value: "0.1234567890123456789" });
    expect(parseValue(col({ type: "numeric" }), "1.5")).toEqual({ ok: true, value: 1.5 });
    expect(parseValue(col({ type: "numeric" }), "0.10")).toEqual({ ok: true, value: "0.10" });
    expect(parseValue(col({ type: "bigint" }), "9007199254740993")).toEqual({ ok: true, value: "9007199254740993" });
    expect(parseValue(col({ type: "bigint" }), "9007199254740991")).toEqual({ ok: true, value: 9007199254740991 });
    expect(buildFilterTree({ mode: "and", rows: [{ col: "n", op: "in", value: "0.1234567890123456789, 2" }] }, { n: col({ type: "numeric" }) })).toEqual({
      ok: true,
      where: { col: "n", in: ["0.1234567890123456789", 2] },
    });
  });
  it("rejects non-finite and non-decimal numbers", () => {
    for (const bad of ["Infinity", "-Infinity", "NaN", "0x10", "1e400", "", "1,5"]) {
      expect(parseValue(col({ type: "numeric" }), bad), bad).toMatchObject({ ok: false });
    }
    for (const bad of ["Infinity", "0x10", "1e3", "1.0"]) {
      expect(parseValue(col({ type: "integer" }), bad), bad).toMatchObject({ ok: false });
    }
  });
  it("whereByKey refuses to build an unfiltered or partial key", () => {
    expect(() => whereByKey([], { id: 1 })).toThrow();
    expect(() => whereByKey(["id"], {})).toThrow();
    expect(() => whereByKey(["id"], { id: undefined })).toThrow();
    expect(() => whereByKey(["a", "b"], { a: 1 })).toThrow();
    expect(() => whereByKey(["id"], { id: null })).toThrow();
  });
});

describe("consoleErrorText", () => {
  it("has plain text for role, availability and unknown codes", () => {
    const e = (code: string, message = "") => new ApiError(400, code as never, message);
    const ctx = { role: "anon", op: "select" as const };
    expect(consoleErrorText(e("role_refused"), ctx)).toMatch(/bypass row-level security/);
    expect(consoleErrorText(e("role_not_allowed"), ctx)).toMatch(/isn't allowed/);
    expect(consoleErrorText(e("data_api_disabled"), ctx)).toMatch(/isn't enabled/);
    expect(consoleErrorText(e("mystery"), ctx)).toBe("Something went wrong (mystery)");
    expect(consoleErrorText(e("raised"), ctx)).toBe("Something went wrong (raised)");
  });
  const err = (status: number, code: string, message = "raw") => new ApiError(status, code as never, message);
  it("error_codes_have_plain_text", () => {
    expect(consoleErrorText(err(403, "permission_denied"), { role: "anon", op: "update" })).toBe(
      "anon can't update this table.",
    );
    expect(consoleErrorText(err(403, "permission_denied"), { role: null, op: "select" })).toBe(
      "The owner can't read this table.",
    );
    expect(consoleErrorText(err(409, "data_api_disabled"), { role: null, op: "select" })).toMatch(/isn't enabled/);
    expect(consoleErrorText(err(404, "unknown_table"), { role: null, op: "select" })).toMatch(/table or view/);
    expect(consoleErrorText(err(403, "role_refused"), { role: "admin", op: "select" })).toMatch(/bypass row-level security/);
    expect(consoleErrorText(err(409, "conflict", "duplicate key"), { role: null, op: "insert" })).toMatch(/duplicate key/);
    expect(consoleErrorText(err(500, "weird", "kept"), { role: null, op: "select" })).toBe("kept");
  });
});

describe("review fixes", () => {
  it("buildSelect pages by the given page size", () => {
    expect(buildSelect({ schema: "p", table: "t", page: 3, sort: null, filters: null, pk: [], pageSize: 20 })).toMatchObject({ limit: 20, offset: 60 });
  });
  it("whereByKey refuses a number that is not a safe integer", () => {
    expect(() => whereByKey(["id"], { id: 9007199254740992 })).toThrow(/id/);
    expect(() => whereByKey(["id"], { id: 1e21 })).toThrow();
    expect(whereByKey(["id"], { id: "9007199254740993" })).toEqual({ col: "id", eq: "9007199254740993" });
    expect(whereByKey(["id"], { id: 9007199254740991 })).toEqual({ col: "id", eq: 9007199254740991 });
  });
  it("json input refuses numbers that do not round-trip", () => {
    const j = col({ type: "jsonb" });
    for (const bad of ["1e400", "9007199254740993", '{"a":[12345678901234567890]}', "0.1234567890123456789"]) {
      expect(parseValue(j, bad), bad).toMatchObject({ ok: false, error: expect.stringMatching(/number/) });
    }
    for (const ok of ["1.5", "1.0", "1e3", "-0.25", '{"a":[1,2.5],"b":"12345678901234567890"}', "9007199254740991"]) {
      expect(parseValue(j, ok), ok).toMatchObject({ ok: true });
    }
  });
  it("like and ilike are offered for text-like columns only", () => {
    expect(filterOpsFor(col({ type: "text" }))).toContain("like");
    expect(filterOpsFor(col({ type: "character varying(20)" }))).toContain("ilike");
    for (const t of ["integer", "boolean", "date", "jsonb", "uuid"]) {
      expect(filterOpsFor(col({ type: t })), t).not.toContain("like");
    }
    expect(filterOpsFor(col({ type: "text", array: true }))).not.toContain("like");
    expect(buildFilterTree({ mode: "and", rows: [{ col: "n", op: "like", value: "1%" }] }, { n: col({ type: "integer" }) })).toMatchObject({ ok: false });
  });
});
