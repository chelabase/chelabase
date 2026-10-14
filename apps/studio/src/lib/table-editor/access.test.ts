import { describe, expect, it } from "vitest";
import type { Branch, BranchDatabaseView, MeResponse } from "@chelabase/management-client";
import { projectCaps } from "@/lib/capabilities";
import { branch, meAs, pgDb } from "@/test/table-editor-fixtures";
import { rowAccess, structureAccess } from "./access";

const caps = (level: "viewer" | "developer" | "admin") => projectCaps(meAs(level) as unknown as MeResponse, "p1");
const main = branch("b1", "main", null, { protected: true }) as unknown as Branch;
const dev = branch("b2", "staging", "b1") as unknown as Branch;
const on = pgDb("app", true) as unknown as BranchDatabaseView;
const off = pgDb("reporting", false, "off") as unknown as BranchDatabaseView;

describe("editor access", () => {
  it("structure_by_role_and_protection", () => {
    expect(structureAccess(caps("admin"), main)).toEqual({ allowed: true });
    expect(structureAccess(caps("developer"), dev)).toEqual({ allowed: true });
    expect(structureAccess(caps("developer"), main)).toEqual({ allowed: false, reason: "main is protected: schema changes need Admin" });
    expect(structureAccess(caps("viewer"), dev)).toEqual({ allowed: false, reason: "Viewers can't change the schema" });
    expect(structureAccess(projectCaps(meAs("admin") as unknown as MeResponse, "p-other"), dev)).toEqual({ allowed: false, reason: "You don't have access to this project" });
  });
  it("rows_by_data_api_role_and_level", () => {
    expect(rowAccess(caps("developer"), { owner: false, roles: ["billing_admin"] }, on)).toEqual({ read: true, write: true, reason: null });
    expect(rowAccess(caps("viewer"), { owner: false, roles: ["reporter"] }, on)).toEqual({ read: true, write: false, reason: null });
    expect(rowAccess(caps("developer"), { owner: false, roles: [] }, on)).toEqual({ read: false, write: false, reason: "You can't act as any Postgres role here." });
    expect(rowAccess(caps("admin"), { owner: true, roles: [] }, off)).toEqual({ read: false, write: false, reason: "Rows need the data API on reporting" });
  });
});
