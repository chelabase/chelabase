import { configureManagementClient } from "@chelabase/management-client";
import { describe, expect, it } from "vitest";
import { fakeFetch } from "../test/fake-fetch";
import { changesSchema, errorOffset, initialRunState, readNdjson, runReducer, startRun, type RunEvent } from "./sql-run";

function streamOf(chunks: Uint8Array[]): ReadableStream<Uint8Array> {
  return new ReadableStream({
    start(c) {
      for (const ch of chunks) c.enqueue(ch);
      c.close();
    },
  });
}
const enc = new TextEncoder();
async function collect(s: ReadableStream<Uint8Array>): Promise<RunEvent[]> {
  const out: RunEvent[] = [];
  for await (const e of readNdjson(s)) out.push(e);
  return out;
}
const reduce = (events: RunEvent[]) => events.reduce(runReducer, initialRunState);

describe("readNdjson", () => {
  it("readNdjson_handles_lines_split_across_chunks", async () => {
    const bytes = enc.encode('{"t":"heartbeat"}\n\n{"t":"notice","severity":"NOTICE","code":"00000","message":"héllo 😀","detail":null,"hint":null,"position":null}\n{"t":"done","outcome":"ok","duration_ms":3}');
    // split inside the 4-byte emoji and inside the first line
    const emoji = bytes.indexOf(0xf0);
    const chunks = [bytes.slice(0, 5), bytes.slice(5, emoji + 2), bytes.slice(emoji + 2)];
    const events = await collect(streamOf(chunks));
    expect(events.map((e) => e.t)).toEqual(["heartbeat", "notice", "done"]);
    expect(events[1]).toMatchObject({ message: "héllo 😀" });
  });

  it("throws_on_invalid_json_with_the_line", async () => {
    await expect(collect(streamOf([enc.encode("not json\n")]))).rejects.toThrow(/not json/);
  });
});

describe("runReducer", () => {
  it("reducer_builds_statements_in_order", () => {
    const s = reduce([
      { t: "columns", statement: 0, columns: [{ name: "a", type: "int4" }] },
      { t: "rows", statement: 0, rows: [["1"], ["2"]] },
      { t: "rows", statement: 0, rows: [[null]] },
      { t: "complete", statement: 0, tag: "SELECT 3", rows: 3, truncated: false },
      { t: "columns", statement: 1, columns: [{ name: "b", type: "text" }] },
      { t: "rows", statement: 1, rows: [["x"]] },
      { t: "complete", statement: 1, tag: "SELECT 1", rows: 1, truncated: true },
      { t: "done", outcome: "ok", duration_ms: 5 },
    ]);
    expect(s.statements.map((x) => x.index)).toEqual([0, 1]);
    expect(s.statements[0]).toMatchObject({ rows: [["1"], ["2"], [null]], tag: "SELECT 3", rowCount: 3, truncated: false });
    expect(s.statements[1].truncated).toBe(true);
    expect(s.running).toBe(false);
  });

  it("complete_without_columns_creates_a_ddl_result", () => {
    const s = reduce([{ t: "complete", statement: 0, tag: "CREATE TABLE", rows: 0, truncated: false }]);
    expect(s.statements).toEqual([{ index: 0, columns: [], rows: [], tag: "CREATE TABLE", rowCount: 0, truncated: false }]);
    expect(s.running).toBe(true);
  });

  it("error_then_done_stops_running", () => {
    const err = { t: "error", statement: 1, code: "42P01", message: "nope", detail: null, hint: null, position: 3 } as const;
    const s = reduce([err, { t: "notice", severity: "NOTICE", code: "0", message: "m", detail: null, hint: null, position: null }, { t: "done", outcome: "error", duration_ms: 1 }]);
    expect(s.error).toEqual(err);
    expect(s.notices).toHaveLength(1);
    expect(s.running).toBe(false);
    expect(s.done?.outcome).toBe("error");
  });

  it("unknown_events_are_ignored", () => {
    const s = runReducer(initialRunState, { t: "future_thing" } as unknown as RunEvent);
    expect(s).toEqual(initialRunState);
    expect(runReducer(initialRunState, { t: "heartbeat" })).toEqual(initialRunState);
  });
});

describe("errorOffset", () => {
  it("positions_count_code_points", () => {
    const sql = "SELECT '😀', nope";
    const off = errorOffset(sql, 13, 0);
    expect(sql.slice(off)).toBe("nope");
  });
  it("selection_offset_is_added", () => {
    expect(errorOffset("SELECT nope", 8, 20)).toBe(27);
  });
});

describe("changesSchema", () => {
  it("changesSchema_detects_ddl_only", () => {
    expect(changesSchema(["CREATE TABLE"])).toBe(true);
    expect(changesSchema(["SELECT 1", "ALTER TABLE"])).toBe(true);
    expect(changesSchema(["INSERT 0 1"])).toBe(false);
    expect(changesSchema(["TRUNCATE TABLE"])).toBe(false);
  });
});

describe("startRun", () => {
  it("posts_the_body_and_yields_events", async () => {
    const f = fakeFetch({
      "POST /v1/databases/d1/branches/b1/sql": () =>
        new Response('{"t":"done","outcome":"ok","duration_ms":1}\n', { status: 200, headers: { "content-type": "application/x-ndjson" } }),
    });
    configureManagementClient({ baseUrl: "https://api.test", getToken: () => "t", onUnauthorized: () => {}, fetchImpl: f as typeof fetch });
    const body = { run_id: "r", database: null, role: null, sql: "select 1", timeout_ms: 1000, confirm: false };
    const events: RunEvent[] = [];
    for await (const e of await startRun("d1", "b1", body, new AbortController().signal)) events.push(e);
    expect(events).toEqual([{ t: "done", outcome: "ok", duration_ms: 1 }]);
    expect(await f.calls[0].json()).toEqual(body);
  });
});
