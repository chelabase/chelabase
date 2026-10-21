import { screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { FakeRealtime } from "@/test/fake-realtime";
import { B, ROWS, filterByTextKey, obj, renderEditor, stubLayout } from "@/test/table-editor-fixtures";

/**
 * RT1 second Playwright pass, ruling 3 (user, 2026-10-21): struck-through deleted rows don't count.
 *
 * Pins for the coder:
 * - After a live keyed delete the row stays on screen struck through (`data-deleted="true"`, as today) but it is not
 *   a row "on screen" for the F4 floor. The footer total ("of ≈ N") and the table bar's "≈ N rows" show
 *   `max(row_estimate, offset + rows on screen that are not struck)`. The range text ("Rows 1–3") is not pinned.
 * - The F4 pins (rows-view-fs.test.tsx) stay: the number never reads below the live rows on screen, and a larger
 *   estimate is left alone.
 * - Edge (user's "32 shown while 31 exist"): with no estimate, the exact count the footer and the table bar derive from
 *   a short page drops the struck rows too.
 * - The count follows the strike: a second delete lowers it again.
 */

const Q = `${B}/console/query`;
const key = (id: string) => JSON.stringify([id]);
type Data = Record<string, unknown>[];

function server(data: Data): FakeHandler {
  return async (req) => {
    const { tree } = await req.clone().json();
    return jsonResponse(tree.op === "select" ? filterByTextKey(tree.where, data) : data);
  };
}
function mount(o: { objects: unknown[] }) {
  const rt = new FakeRealtime();
  const data: Data = ROWS.map((r) => ({ ...r }));
  const s = renderEditor({ realtime: rt, objects: o.objects, routes: { [`POST ${Q}`]: server(data) } });
  return { rt, data, ...s };
}
const est = (row_estimate: number | null) => [obj("public", "invoices", { row_estimate, total_bytes: 12582912 })];
const header = () => document.querySelector('h2[title="public.invoices"]')?.parentElement?.textContent ?? "";
const struck = () => document.querySelectorAll('[role="row"][data-deleted="true"]').length;
const strike = async (rt: FakeRealtime, ...ids: string[]) => {
  rt.rows(1, ids.map((id) => ({ op: "delete" as const, keys: [key(id)] })));
  await vi.waitFor(() => expect(struck()).toBe(ids.length));
};
// joined, but no `joined` answer: it would read the page again and drop the struck rows
const joined = (rt: FakeRealtime) => vi.waitFor(() => expect(rt.active.length).toBeGreaterThan(0));

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
  stubLayout();
  vi.stubGlobal("matchMedia", (q: string) => ({ matches: false, media: q, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {}, onchange: null, dispatchEvent: () => false }));
});
afterEach(() => vi.unstubAllGlobals());

describe("ruling 3: struck rows are not rows on screen", () => {
  it("a_live_delete_lowers_the_footer_and_the_header_below_the_old_floor", async () => {
    const { rt } = mount({ objects: est(2) });
    await joined(rt);
    // 3 rows on screen against an estimate of 2: the F4 floor reads 3
    expect(await screen.findByText("Rows 1–3 of ≈ 3")).toBeInTheDocument();
    expect(header()).toContain("≈ 3 rows");
    await strike(rt, "c2d1-3");
    expect(await screen.findByText(/^Rows 1–\d of ≈ 2$/)).toBeInTheDocument();
    expect(header()).toContain("≈ 2 rows");
    expect(header()).not.toContain("≈ 3 rows");
  });

  it("every_further_delete_lowers_it_again_down_to_the_estimate", async () => {
    const { rt } = mount({ objects: est(1) });
    await joined(rt);
    await screen.findByText("Rows 1–3 of ≈ 3");
    await strike(rt, "c2d1-3", "81c4-2");
    expect(await screen.findByText(/^Rows 1–\d of ≈ 1$/)).toBeInTheDocument();
    expect(header()).toContain("≈ 1 rows");
  });

  it("an_estimate_above_the_live_rows_is_left_alone_after_a_delete", async () => {
    const { rt } = mount({ objects: est(48210) });
    await joined(rt);
    await screen.findByText("Rows 1–3 of ≈ 48,210");
    await strike(rt, "c2d1-3");
    expect(screen.getByText(/^Rows 1–\d of ≈ 48,210$/)).toBeInTheDocument();
    expect(header()).toContain("≈ 48,210 rows");
  });

  it("without_an_estimate_the_exact_count_drops_the_struck_rows_too", async () => {
    const { rt } = mount({ objects: est(null) });
    await joined(rt);
    expect(await screen.findByText("Rows 1–3 of 3")).toBeInTheDocument();
    expect(header()).toContain("3 rows");
    await strike(rt, "c2d1-3");
    expect(await screen.findByText(/^Rows 1–\d of 2$/)).toBeInTheDocument();
    expect(header()).toContain("2 rows");
    expect(header()).not.toContain("3 rows");
  });
});
