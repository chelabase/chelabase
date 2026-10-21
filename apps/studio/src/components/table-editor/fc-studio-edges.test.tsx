import { act, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FakeRealtime } from "@/test/fake-realtime";
import { renderEditor, stubLayout } from "@/test/table-editor-fixtures";

// RT1 final round, batch C: the coder's edge cases beyond the RED set (Studio side).

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

const grid = () => screen.findByRole("grid", { name: "public.invoices" });

describe("live badge on non-fatal errors (edges)", () => {
  it("a_transient_refusal_before_the_join_says_why_and_the_retried_join_turns_it_live", async () => {
    const rt = new FakeRealtime();
    renderEditor({ realtime: rt });
    await within(await grid()).findByRole("gridcell", { name: "45000" });
    act(() => rt.table.handlers.onError?.({ code: "limit", message: "x" }));
    expect(screen.getByTestId("live-state")).toHaveTextContent("Live updates paused · too many live tables");
    act(() => rt.joined());
    expect(screen.getByTestId("live-state")).toHaveTextContent(/^Live$/);
  });

  it("a_non_fatal_error_while_paused_keeps_the_pause_and_its_reason", async () => {
    const rt = new FakeRealtime();
    renderEditor({ realtime: rt });
    await within(await grid()).findByRole("gridcell", { name: "45000" });
    act(() => rt.joined({ live: false, reason: "no_slot" }));
    act(() => rt.table.handlers.onError?.({ code: "invalid", message: "x" }));
    expect(screen.getByTestId("live-state")).toHaveTextContent("Live updates paused · no free slot");
  });
});
