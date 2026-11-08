import { describe, expect, it } from "vitest";
import {
  STORAGE_EVENTS,
  TABLE_EVENTS,
  initialTriggerDraft,
  splitQualified,
  triggerDraftReducer,
  triggerReason,
  triggerRequest,
  triggerTarget,
} from "./functions-triggers";

describe("splitQualified", () => {
  it("defaults_to_public_and_strips_quotes", () => {
    expect(splitQualified("orders")).toEqual({ schema: "public", table: "orders" });
    expect(splitQualified(" sales.orders ")).toEqual({ schema: "sales", table: "orders" });
    expect(splitQualified('"My.Schema"."Order Lines"')).toEqual({ schema: "My.Schema", table: "Order Lines" });
    expect(splitQualified("")).toBeNull();
    expect(splitQualified("a.b.c")).toBeNull();
    expect(splitQualified("sales.")).toBeNull();
  });
});

describe("triggerDraftReducer", () => {
  it("starts_as_a_table_trigger_on_every_table_event", () => {
    expect(initialTriggerDraft("app")).toEqual({ source: "table", pgDatabase: "app", table: "", bucket: "", prefix: "", events: [...TABLE_EVENTS] });
  });

  it("switching_source_resets_events_to_that_sources_set", () => {
    let d = triggerDraftReducer(initialTriggerDraft("app"), { type: "source", source: "storage" });
    expect(d.events).toEqual([...STORAGE_EVENTS]);
    d = triggerDraftReducer(d, { type: "toggle-event", event: "object.moved" });
    d = triggerDraftReducer(d, { type: "source", source: "table" });
    expect(d.events).toEqual([...TABLE_EVENTS]);
    // The same source keeps the choice.
    const e = triggerDraftReducer(d, { type: "toggle-event", event: "delete" });
    expect(triggerDraftReducer(e, { type: "source", source: "table" })).toBe(e);
  });

  it("toggle_event_adds_and_removes_in_canonical_order", () => {
    let d = triggerDraftReducer(initialTriggerDraft("app"), { type: "toggle-event", event: "insert" });
    expect(d.events).toEqual(["update", "delete"]);
    d = triggerDraftReducer(d, { type: "toggle-event", event: "insert" });
    expect(d.events).toEqual(["insert", "update", "delete"]);
    // An event of the other source is ignored.
    expect(triggerDraftReducer(d, { type: "toggle-event", event: "object.created" })).toBe(d);
  });

  it("set_changes_one_field", () => {
    const d = triggerDraftReducer(initialTriggerDraft("app"), { type: "set", field: "table", value: "public.orders" });
    expect(d.table).toBe("public.orders");
  });

  it("reset_keeps_the_database", () => {
    const d = triggerDraftReducer({ ...initialTriggerDraft("app"), table: "x", pgDatabase: "other" }, { type: "reset" });
    expect(d).toEqual(initialTriggerDraft("other"));
  });
});

describe("triggerReason", () => {
  const table = { ...initialTriggerDraft("app"), table: "public.orders" };
  const storage = { ...initialTriggerDraft("app"), source: "storage" as const, events: [...STORAGE_EVENTS] };

  it("accepts_complete_drafts", () => {
    expect(triggerReason(table)).toBeNull();
    expect(triggerReason(storage)).toBeNull();
    expect(triggerReason({ ...storage, bucket: "avatars", prefix: "users/" })).toBeNull();
  });

  it("refuses_obvious_gaps", () => {
    expect(triggerReason({ ...table, pgDatabase: "" })).toMatch(/Postgres database/);
    expect(triggerReason({ ...table, table: "" })).toMatch(/table/);
    expect(triggerReason({ ...table, table: "a.b.c" })).toMatch(/schema\.table/);
    expect(triggerReason({ ...table, events: [] })).toMatch(/event/);
    expect(triggerReason({ ...storage, events: [] })).toMatch(/event/);
  });
});

describe("triggerRequest", () => {
  it("builds_a_table_request", () => {
    expect(triggerRequest({ ...initialTriggerDraft("app"), table: "sales.orders", events: ["insert"] })).toEqual({
      source: "table",
      pg_database: "app",
      schema: "sales",
      table: "orders",
      events: ["insert"],
    });
  });

  it("builds_a_storage_request_leaving_blank_filters_out", () => {
    const d = { ...initialTriggerDraft("app"), source: "storage" as const, events: ["object.created"], bucket: " ", prefix: "" };
    expect(triggerRequest(d)).toEqual({ source: "storage", pg_database: "app", events: ["object.created"] });
    expect(triggerRequest({ ...d, bucket: "avatars", prefix: "u/" })).toEqual({ source: "storage", pg_database: "app", events: ["object.created"], bucket: "avatars", prefix: "u/" });
  });
});

describe("triggerTarget", () => {
  it("names_the_table_or_storage_scope", () => {
    expect(triggerTarget({ source: "table", pg_database: "app", schema: "public", table: "orders", bucket: null, prefix: null } as never)).toBe("app · public.orders");
    expect(triggerTarget({ source: "storage", pg_database: "app", schema: null, table: null, bucket: null, prefix: null } as never)).toBe("app · every bucket");
    expect(triggerTarget({ source: "storage", pg_database: "app", schema: null, table: null, bucket: "avatars", prefix: "u/" } as never)).toBe("app · avatars/u/*");
    expect(triggerTarget({ source: "storage", pg_database: "app", schema: null, table: null, bucket: null, prefix: "u/" } as never)).toBe("app · every bucket, u/*");
  });
});
