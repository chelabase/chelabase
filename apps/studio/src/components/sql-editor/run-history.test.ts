import { afterEach, describe, expect, it, vi } from "vitest";
import { addHistory, clearHistory, loadBuffer, loadHistory, loadTimeout, saveBuffer, saveTimeout, type HistoryEntry } from "./run-history";

const entry = (n: number): HistoryEntry => ({ sql: `select ${n}`, branchId: "b1", database: null, role: null, at: `2026-10-03T00:00:${String(n % 60).padStart(2, "0")}Z`, outcome: "ok", durationMs: n });

afterEach(() => {
  localStorage.clear();
  vi.restoreAllMocks();
});

describe("run history", () => {
  it("history_keeps_100_newest", () => {
    for (let i = 1; i <= 105; i++) addHistory("u1", "d1", entry(i));
    const list = loadHistory("u1", "d1");
    expect(list).toHaveLength(100);
    expect(list[0].sql).toBe("select 105");
    expect(list[99].sql).toBe("select 6");
    expect(localStorage.getItem("cheladb.sql.history.u1.d1")).not.toBeNull();
    expect(loadHistory("u1", "d2")).toEqual([]);
    clearHistory("u1", "d1");
    expect(loadHistory("u1", "d1")).toEqual([]);
  });

  it("buffer_and_timeout_round_trip", () => {
    saveBuffer("u1", "d1", "select 1");
    expect(loadBuffer("u1", "d1")).toBe("select 1");
    expect(loadTimeout()).toBe(60000);
    saveTimeout(300000);
    expect(localStorage.getItem("cheladb.sql.timeout")).toBe("300000");
    expect(loadTimeout()).toBe(300000);
    localStorage.setItem("cheladb.sql.timeout", "12");
    expect(loadTimeout()).toBe(60000);
  });

  it("history_survives_storage_throwing", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("quota");
    });
    vi.spyOn(Storage.prototype, "removeItem").mockImplementation(() => {
      throw new Error("denied");
    });
    expect(loadHistory("u1", "d1")).toEqual([]);
    expect(addHistory("u1", "d1", entry(1))).toEqual([entry(1)]);
    expect(() => clearHistory("u1", "d1")).not.toThrow();
    expect(loadBuffer("u1", "d1")).toBe("");
    expect(() => saveBuffer("u1", "d1", "x")).not.toThrow();
    expect(loadTimeout()).toBe(60000);
    expect(() => saveTimeout(300000)).not.toThrow();
  });

  it("ignores_corrupt_history", () => {
    localStorage.setItem("cheladb.sql.history.u1.d1", "{nope");
    expect(loadHistory("u1", "d1")).toEqual([]);
    localStorage.setItem("cheladb.sql.history.u1.d1", JSON.stringify([{ bad: 1 }, entry(2)]));
    expect(loadHistory("u1", "d1")).toEqual([entry(2)]);
  });
});
