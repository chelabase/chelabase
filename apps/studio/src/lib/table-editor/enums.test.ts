import { describe, expect, it } from "vitest";
import type { Operation, TypeEntry } from "@chelabase/management-client";
import { projectEnums } from "./enums";

const status = { oid: 1, array_oid: 2, schema: "public", name: "status", kind: "enum", category: "E", preferred: false, takes_modifiers: false, labels: ["open", "paid", "void"] } as TypeEntry;
const other = { ...status, schema: "billing", name: "kind" } as TypeEntry;
const names = (ops: Operation[]) => projectEnums([status, other], ops, "public").map((e) => [e.name, e.mark, e.values.map((v) => v.value)]);

describe("projectEnums", () => {
  it("lists the schema's enums with their labels", () => {
    expect(names([])).toEqual([["status", null, ["open", "paid", "void"]]]);
  });

  it("places added values and follows renames", () => {
    const ops: Operation[] = [
      { add_enum_value: { schema: "public", name: "status", value: "refunded", position: { after: "paid" } } },
      { rename_enum_value: { schema: "public", name: "status", from: "void", to: "cancelled" } },
      { rename_enum: { schema: "public", name: "status", to: "state" } },
      { add_enum_value: { schema: "public", name: "state", value: "new", position: { before: "open" } } },
    ];
    expect(names(ops)).toEqual([["state", "changed", ["new", "open", "paid", "refunded", "cancelled"]]]);
  });

  it("creates and drops enums; a created one that is dropped leaves nothing", () => {
    const ops: Operation[] = [
      { create_enum: { schema: "public", name: "tier", values: ["free"] } },
      { drop_enum: { schema: "public", name: "status" } },
      { create_enum: { schema: "public", name: "gone", values: [] } },
      { drop_enum: { schema: "public", name: "gone" } },
      { create_enum: { schema: "billing", name: "elsewhere", values: [] } },
    ];
    expect(names(ops)).toEqual([["status", "dropped", ["open", "paid", "void"]], ["tier", "added", ["free"]]]);
  });
});
