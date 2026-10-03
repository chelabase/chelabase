import { CompletionContext, type CompletionResult, type CompletionSource } from "@codemirror/autocomplete";
import { EditorState } from "@codemirror/state";
import { describe, expect, it, vi } from "vitest";
import type { PglsClient, PglsCompletion } from "./client";
import { hoverText, pglsCompletionSource, pglsLintSource, sqlExtensions } from "./codemirror";

function fakeClient(completions: PglsCompletion[] = []): PglsClient & { calls: string[] } {
  const calls: string[] = [];
  return {
    calls,
    ready: Promise.resolve(),
    setSchema: vi.fn(async () => {}),
    update: vi.fn(async (text: string) => void calls.push(`update:${text}`)),
    complete: vi.fn(async (offset: number) => {
      calls.push(`complete:${offset}`);
      return completions;
    }),
    lint: vi.fn(async () => [
      { start: 0, end: 5, message: "bad", severity: "error" as const },
      { start: 6, end: 7, message: "note", severity: "hint" as const },
    ]),
    hover: vi.fn(async () => null),
    dispose: vi.fn(),
  };
}

function context(doc: string, pos = doc.length, explicit = false) {
  const state = EditorState.create({ doc, extensions: sqlExtensions(null) });
  return new CompletionContext(state, pos, explicit);
}

const COMPLETIONS: PglsCompletion[] = [
  { label: "users", kind: "table", detail: "public" },
  { label: "email", kind: "column" },
  { label: "now", kind: "function" },
  { label: "public", kind: "schema" },
  { label: "select", kind: "keyword" },
  { label: "anon", kind: "role" },
  { label: "owner_only", kind: "policy" },
];

describe("sqlExtensions", () => {
  it("completion_replaces_the_word_at_the_cursor", async () => {
    const client = fakeClient(COMPLETIONS);
    const doc = "select * from us";
    const result = (await pglsCompletionSource(client)(context(doc))) as CompletionResult;
    expect(result.from).toBe(doc.length - 2);
    expect(result.to ?? doc.length).toBe(doc.length);
    expect(client.calls).toEqual([`update:${doc}`, `complete:${doc.length}`]);
    expect(result.options.map((o) => [o.label, o.type, o.detail])).toEqual([
      ["users", "class", "public"],
      ["email", "property", undefined],
      ["now", "function", undefined],
      ["public", "namespace", undefined],
      ["select", "keyword", undefined],
      ["anon", "variable", undefined],
      ["owner_only", "variable", undefined],
    ]);
  });

  it("completion_after_a_dot_starts_at_the_dot", async () => {
    const client = fakeClient(COMPLETIONS);
    const doc = "select * from public.";
    // Implicit (typed) completion still fires right after the dot.
    const result = (await pglsCompletionSource(client)(context(doc))) as CompletionResult;
    expect(result.from).toBe(doc.length);
    const typed = (await pglsCompletionSource(client)(context(`${doc}us`))) as CompletionResult;
    expect(typed.from).toBe(doc.length);
  });

  it("does not pop up on whitespace unless asked", async () => {
    const client = fakeClient(COMPLETIONS);
    expect(await pglsCompletionSource(client)(context("select * from "))).toBeNull();
    const explicit = (await pglsCompletionSource(client)(context("select * from ", 14, true))) as CompletionResult;
    expect(explicit.from).toBe(14);
  });

  it("returns no completions when the client fails", async () => {
    const client = fakeClient();
    client.complete = async () => {
      throw new Error("wasm gone");
    };
    expect(await pglsCompletionSource(client)(context("select x", 8, true))).toBeNull();
  });

  it("ranks_system_catalog_items_below_the_users", async () => {
    const client = fakeClient([
      { label: "normalize", kind: "function", detail: "Schema: pg_catalog" },
      { label: "notes", kind: "table", detail: "public" },
      { label: "columns", kind: "table", detail: "information_schema" },
      { label: "pg_catalog", kind: "schema" },
      { label: "now", kind: "function", detail: "Schema: pg_catalog" },
      { label: "note_count", kind: "function", detail: "Schema: app" },
    ]);
    const result = (await pglsCompletionSource(client)(context("select * from no"))) as CompletionResult;
    const boost = Object.fromEntries(result.options.map((o) => [o.label, o.boost ?? 0]));
    expect(boost.notes).toBe(0);
    expect(boost.note_count).toBe(0);
    for (const label of ["normalize", "columns", "pg_catalog", "now"]) expect(boost[label], label).toBeLessThan(0);
  });

  it("hover_markdown_becomes_plain_text", () => {
    const md = "### `public.notes` - RLS disabled\n```plain\nColumns:\n- id: int4 - not null\n\n```\n---\n```plain\n\n~2 rows, `x` kB\n```";
    expect(hoverText(md)).toBe("public.notes - RLS disabled\nColumns:\n- id: int4 - not null\n\n---\n\n~2 rows, x kB");
    // Markup stays text: nothing is parsed as HTML.
    expect(hoverText("# <b>x</b>")).toBe("<b>x</b>");
  });

  it("maps hint diagnostics to info", async () => {
    const client = fakeClient();
    const state = EditorState.create({ doc: "selec 1;" });
    const diags = await pglsLintSource(client)({ state } as never);
    expect(diags).toEqual([
      { from: 0, to: 5, severity: "error", message: "bad", source: "pgls" },
      { from: 6, to: 7, severity: "info", message: "note", source: "pgls" },
    ]);
  });

  it("null_client_uses_keyword_completion", async () => {
    const doc = "sel";
    const state = EditorState.create({ doc, extensions: sqlExtensions(null) });
    const sources = state.languageDataAt<CompletionSource>("autocomplete", doc.length);
    expect(sources.length).toBeGreaterThan(0);
    const ctx = new CompletionContext(state, doc.length, true);
    const labels: string[] = [];
    for (const source of sources) {
      const r = await source(ctx);
      if (r) labels.push(...r.options.map((o) => o.label.toLowerCase()));
    }
    expect(labels).toContain("select");
  });

  it("pgls mode drops lang-sql's keyword completion for pgls's", () => {
    const state = EditorState.create({ doc: "sel", extensions: sqlExtensions(fakeClient()) });
    const sources = state.languageDataAt<CompletionSource>("autocomplete", 3);
    expect(sources).toHaveLength(1);
  });
});
