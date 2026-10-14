import { ApiError } from "@chelabase/management-client";
import { describe, expect, it } from "vitest";
import { CATALOG, invoices } from "./test-tables";
import { CELL_LIMIT, WIDTHS_KEY, cellText, columnDescs, editText, isTooLarge, primaryKeyOf, readWidths, temporalKind, toTsv, validTimestamptz, writeWidths } from "./rows";

describe("rows helpers", () => {
  it("cell_text_shows_null_json_and_cuts_long_values", () => {
    expect(cellText(null)).toBe("NULL");
    expect(cellText(undefined)).toBe("NULL");
    expect(cellText(true)).toBe("true");
    expect(cellText({ a: [1] })).toBe('{"a":[1]}');
    expect(cellText("x".repeat(CELL_LIMIT))).toBe("x".repeat(CELL_LIMIT));
    expect(cellText("x".repeat(CELL_LIMIT + 1))).toBe(`${"x".repeat(CELL_LIMIT)}…`);
  });

  it("edit_text_is_empty_for_null_and_json_for_objects", () => {
    expect(editText(null)).toBe("");
    expect(editText(12)).toBe("12");
    expect(editText(["a"])).toBe('["a"]');
  });

  it("tsv_has_a_header_and_quotes_fields_with_tabs_newlines_or_quotes", () => {
    const rows = [{ a: "x\ty", b: null }, { a: 'say "hi"', b: { k: 1 } }];
    expect(toTsv(["a", "b"], rows)).toBe('a\tb\n"x\ty"\t\n"say ""hi"""\t"{""k"":1}"');
  });

  it("column_descs_follow_the_table_and_the_type_catalog", () => {
    const descs = columnDescs(invoices(), [...CATALOG] as never);
    expect(descs.map((d) => d.name)).toEqual(["id", "customer_id", "status", "amount_cents", "memo"]);
    expect(descs[2].desc).toMatchObject({ type: "invoice_status", enum: "public.invoice_status", nullable: false });
    expect(descs[2].labels).toEqual(["open", "paid", "void"]);
    expect(descs[3].desc).toMatchObject({ type: "bigint", array: false, enum: null });
    expect(descs[4].desc.nullable).toBe(true);
  });

  it("primary_key_names_its_columns_in_constraint_order", () => {
    expect(primaryKeyOf(invoices())).toEqual(["id"]);
    expect(primaryKeyOf(invoices({ constraints: [] }))).toEqual([]);
  });

  it("temporal_kinds_follow_the_type_name", () => {
    const d = (type: string) => ({ type, array: false, enum: null, generated: false, has_default: false, nullable: true });
    expect(temporalKind(d("date"))).toBe("date");
    expect(temporalKind(d("time without time zone"))).toBe("time");
    expect(temporalKind(d("time(3)"))).toBe("time");
    expect(temporalKind(d("timestamp without time zone"))).toBe("timestamp");
    expect(temporalKind(d("timestamp(6) without time zone"))).toBe("timestamp");
    expect(temporalKind(d("timestamp with time zone"))).toBe("timestamptz");
    expect(temporalKind(d("timestamptz"))).toBe("timestamptz");
    expect(temporalKind(d("time with time zone"))).toBeNull();
    expect(temporalKind(d("text"))).toBeNull();
    expect(temporalKind({ ...d("date"), array: true })).toBeNull();
  });

  it("timestamptz_text_is_validated", () => {
    for (const ok of ["2026-10-01 12:00:00+00", "2026-10-01T12:00:00.123456+02:00", "2026-10-01T12:00Z", "2026-10-01 12:00", "infinity", "-infinity"]) {
      expect(validTimestamptz(ok)).toBe(true);
    }
    for (const bad of ["", "tomorrow", "2026-10-01", "12:00", "2026-13-01 12:00"]) expect(validTimestamptz(bad)).toBe(false);
  });

  it("too_large_is_a_413_or_its_code", () => {
    expect(isTooLarge(new ApiError(413, "internal" as never, "big"))).toBe(true);
    expect(isTooLarge(new ApiError(400, "too_large" as never, "big"))).toBe(true);
    expect(isTooLarge(new ApiError(403, "permission_denied", "no"))).toBe(false);
    expect(isTooLarge(new Error("x"))).toBe(false);
  });

  it("widths_are_kept_per_table_and_storage_errors_are_ignored", () => {
    localStorage.clear();
    writeWidths(localStorage, "t1", { a: 120 });
    writeWidths(localStorage, "t2", { b: 90 });
    expect(readWidths(localStorage, "t1")).toEqual({ a: 120 });
    expect(JSON.parse(localStorage.getItem(WIDTHS_KEY)!)).toEqual({ t1: { a: 120 }, t2: { b: 90 } });
    localStorage.setItem(WIDTHS_KEY, "not json");
    expect(readWidths(localStorage, "t1")).toEqual({});
    expect(readWidths(null, "t1")).toEqual({});
    const broken = { getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("denied"); } } as unknown as Storage;
    expect(readWidths(broken, "t1")).toEqual({});
    expect(() => writeWidths(broken, "t1", { a: 1 })).not.toThrow();
  });
});
