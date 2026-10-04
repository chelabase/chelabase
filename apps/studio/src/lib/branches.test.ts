import { describe, expect, it } from "vitest";
import type { Branch } from "@chelabase/management-client";
import {
  actionReasons,
  buildTree,
  parseSettings,
  pointToUtc,
  restoreBounds,
  validateBranchName,
} from "./branches";

let n = 0;
function branch(over: Partial<Branch> & { name: string }): Branch {
  n += 1;
  return {
    id: `b-${over.name}`,
    parent_branch_id: null,
    kind: "normal",
    status: "ready",
    host: `ep-${over.name}.localtest.me`,
    port: 4432,
    branched_at_time: null,
    branched_at_lsn: null,
    history_start: null,
    created_at: `2026-09-30T10:${String(n).padStart(2, "0")}:00Z`,
    error: null,
    protected: false,
    main_database_missing: false,
    ...over,
  };
}
const main = branch({ name: "main" });
const child = (name: string, parent: Branch, over: Partial<Branch> = {}) =>
  branch({ name, parent_branch_id: parent.id, ...over });

describe("buildTree", () => {
  it("puts main first, children indented under their parent, backups included", () => {
    const dev = child("dev", main);
    const backup = child("main-before-restore-20260930T100000Z", main, { kind: "backup", host: null });
    const feat = child("feat", dev);
    const other = child("other", main);
    const rows = buildTree([feat, other, backup, dev, main]);
    expect(rows.map((r) => [r.branch.name, r.depth])).toEqual([
      ["main", 0],
      ["dev", 1],
      ["feat", 2],
      ["main-before-restore-20260930T100000Z", 1],
      ["other", 1],
    ]);
  });

  it("names the parent of each row and hides deleted branches", () => {
    const dev = child("dev", main);
    const gone = child("gone", main, { status: "deleted" });
    const rows = buildTree([main, dev, gone]);
    expect(rows.map((r) => r.branch.name)).toEqual(["main", "dev"]);
    expect(rows[0].parentName).toBeNull();
    expect(rows[1].parentName).toBe("main");
  });
});

describe("actionReasons", () => {
  it("refuses reset-to-parent and delete on main, but allows restore", () => {
    const r = actionReasons(main, [main]);
    expect(r.resetToParent).toBe("main can't be reset to a parent");
    expect(r.delete).toBe("main can't be deleted; delete the database instead");
    expect(r.restore).toBeNull();
    expect(r.connect).toBeNull();
    expect(r.resetPassword).toBeNull();
  });

  it("lists live normal children as the reason, ignoring backups and deleted ones", () => {
    const a = child("a", main);
    const b = child("b", main);
    const backup = child("bk", main, { kind: "backup", host: null });
    const gone = child("gone", main, { status: "deleted" });
    const all = [main, a, b, backup, gone];
    const r = actionReasons(main, all);
    expect(r.restore).toBe("has child branches: a, b");
    expect(actionReasons(a, all).delete).toBeNull();
    const withKid = [...all, child("kid", a)];
    expect(actionReasons(a, withKid).delete).toBe("has child branches: kid");
    expect(actionReasons(a, withKid).resetToParent).toBe("has child branches: kid");
  });

  it("a backup can't connect; it can be deleted", () => {
    const backup = child("bk", main, { kind: "backup", host: null });
    const r = actionReasons(backup, [main, backup]);
    expect(r.connect).toBe("restore backup: create a branch from it to connect");
    expect(r.resetPassword).toBe("restore backup: create a branch from it to connect");
    expect(r.resetToParent).not.toBeNull();
    expect(r.restore).not.toBeNull();
    expect(r.delete).toBeNull();
  });

  it("a branch that isn't ready can't connect, reset or restore", () => {
    const dev = child("dev", main, { status: "provisioning" });
    const r = actionReasons(dev, [main, dev]);
    expect(r.connect).toBe("branch is not ready (it is provisioning)");
    expect(r.resetToParent).toBe("branch is not ready (it is provisioning)");
    expect(r.restore).toBe("branch is not ready (it is provisioning)");
  });
});

describe("validateBranchName", () => {
  it("accepts 1-63 characters of a-z 0-9 _ -, starting with a letter or digit", () => {
    for (const ok of ["dev", "a", "0abc", "feat_1-x", "a".repeat(63)]) {
      expect(validateBranchName(ok)).toBeNull();
    }
  });
  it("rejects the rest and reserves main", () => {
    for (const bad of ["", "Dev", "_x", "-x", "a b", "a".repeat(64), "é"]) {
      expect(validateBranchName(bad)).not.toBeNull();
    }
    expect(validateBranchName("main")).toMatch(/reserved/);
  });
});

describe("restore bounds", () => {
  const now = new Date(2026, 8, 30, 12, 34, 56);
  it("run from now minus the window (rounded up to the minute) to now", () => {
    expect(restoreBounds(now, 3600)).toEqual({ min: "2026-09-30T11:35", max: "2026-09-30T12:34" });
    expect(restoreBounds(now, 7 * 86400).min).toBe("2026-09-23T12:35");
  });
  it("converts a local value to UTC", () => {
    const r = pointToUtc("2026-09-30T12:00", now, 3600 * 24);
    expect(r).toEqual({ ok: true, iso: new Date(2026, 8, 30, 12, 0).toISOString() });
  });
  it("refuses a value outside the bounds or malformed", () => {
    expect(pointToUtc("2026-09-30T13:00", now, 3600 * 24)).toMatchObject({ ok: false });
    expect(pointToUtc("2026-09-28T12:00", now, 3600 * 24)).toMatchObject({ ok: false });
    expect(pointToUtc("", now, 3600)).toMatchObject({ ok: false });
  });
  it("start no earlier than the branch's history begins", () => {
    const start = new Date(2026, 8, 30, 10, 0, 30).toISOString();
    expect(restoreBounds(now, 7 * 86400, start)).toEqual({ min: "2026-09-30T10:01", max: "2026-09-30T12:34" });
    // A window that ends later than the history's start still wins.
    expect(restoreBounds(now, 3600, start).min).toBe("2026-09-30T11:35");
    expect(pointToUtc("2026-09-30T09:00", now, 7 * 86400, start)).toEqual({
      ok: false,
      error: "That time is before the branch's history begins.",
    });
    expect(pointToUtc("2026-09-30T10:01", now, 7 * 86400, start)).toMatchObject({ ok: true });
  });
  it("use only the window when the history's start is unknown", () => {
    expect(restoreBounds(now, 7 * 86400, null).min).toBe("2026-09-23T12:35");
    expect(pointToUtc("2026-09-25T09:00", now, 7 * 86400, null)).toMatchObject({ ok: true });
  });
});

describe("parseSettings", () => {
  const base = {
    window: "1",
    unit: "days" as "days" | "hours",
    maxBranches: "5",
    maxRows: "1000",
    statementTimeoutMs: "8000",
    maxConnections: "5",
  };
  const parse = (over: Partial<typeof base> = {}) => parseSettings({ ...base, ...over });

  it("converts hours and days to seconds and carries the data API settings", () => {
    expect(parse({ window: "12", unit: "hours" })).toEqual({
      ok: true,
      settings: {
        history_window: 43200,
        max_branches: 5,
        max_rows: 1000,
        statement_timeout_ms: 8000,
        max_connections: 5,
      },
    });
    expect(parse({ window: "30", maxBranches: "0" })).toMatchObject({
      ok: true,
      settings: { history_window: 2592000, max_branches: 0 },
    });
  });
  it("enforces 1 hour to 30 days and 0 to 20 branches", () => {
    expect(parse({ window: "0", unit: "hours" })).toMatchObject({ ok: false });
    expect(parse({ window: "31" })).toMatchObject({ ok: false });
    expect(parse({ window: "721", unit: "hours" })).toMatchObject({ ok: false });
    expect(parse({ window: "1.5" })).toMatchObject({ ok: false });
    expect(parse({ maxBranches: "21" })).toMatchObject({ ok: false });
    expect(parse({ maxBranches: "-1" })).toMatchObject({ ok: false });
    expect(parse({ maxBranches: "" })).toMatchObject({ ok: false });
  });
  it("enforces max_rows 1 to 100000", () => {
    expect(parse({ maxRows: "1" })).toMatchObject({ ok: true });
    expect(parse({ maxRows: "100000" })).toMatchObject({ ok: true });
    for (const v of ["0", "100001", "", "1.5", "-1"]) expect(parse({ maxRows: v })).toMatchObject({ ok: false, maxRows: expect.any(String) });
  });
  it("enforces statement_timeout_ms 100 to 120000", () => {
    expect(parse({ statementTimeoutMs: "100" })).toMatchObject({ ok: true });
    expect(parse({ statementTimeoutMs: "120000" })).toMatchObject({ ok: true });
    for (const v of ["99", "120001", "", "abc"]) expect(parse({ statementTimeoutMs: v })).toMatchObject({ ok: false, statementTimeoutMs: expect.any(String) });
  });
  it("enforces max_connections 1 to 50", () => {
    expect(parse({ maxConnections: "1" })).toMatchObject({ ok: true });
    expect(parse({ maxConnections: "50" })).toMatchObject({ ok: true });
    for (const v of ["0", "51", "", "2.5"]) expect(parse({ maxConnections: v })).toMatchObject({ ok: false, maxConnections: expect.any(String) });
  });
});
