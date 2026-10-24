import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { expectThemedSelects } from "@/test/themed";
import { CHANGES, ROLE, changeBodies, mountPage, type ChangeBody } from "./test-setup";

/** Fix S, finish-review fix 3 (a refused review keeps its evidence) and fix 7 (New tables grid, themed select). */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

async function loaded(opts: Parameters<typeof mountPage>[0] = {}) {
  const m = mountPage(opts);
  await screen.findByRole("table", { name: "Tables and views in billing" });
  return m;
}
const box = (label: string) => screen.getByRole("checkbox", { name: label });
const pending = () => screen.getByRole("complementary", { name: "Pending changes" });
type Handler = (body: ChangeBody) => Response | Promise<Response>;
const onChanges = (fn: Handler): Record<string, FakeHandler> => ({ [`POST ${CHANGES}`]: async (req) => fn((await req.clone().json()) as ChangeBody) });
const statements = (sql: string[]) => sql.map((s, i) => ({ op_index: i, sql: s, executed: false, lock: "none" }));
const dryOk = (sql: string[]) => jsonResponse({ committed: false, dry_run: true, would_succeed: true, data_api_exposure: [], statements: statements(sql) });
const refuse = (extra: object) =>
  jsonResponse({ error: { code: "exposes_data_api", message: `operation 2: every row for "${ROLE}" would reach the data API; nothing was changed`, ...extra } }, 422);

const SQL = ["GRANT DELETE ON billing.customers TO billing_editor;", "GRANT EXECUTE ON FUNCTION billing.recalc_totals(uuid) TO billing_editor;"];

/** Stages two ordinary changes and then "All rows on refunds" (change 3), and opens the review. */
async function reviewThree(routes: Record<string, FakeHandler>) {
  const m = await loaded({ routes });
  await m.user.click(box("DELETE on customers"));
  await m.user.click(box("EXECUTE on billing.recalc_totals(uuid)"));
  await m.user.click(screen.getByRole("switch", { name: "All rows on refunds" }));
  expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 3");
  await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
  return m;
}

describe("a refused review keeps its evidence (fix 3)", () => {
  const refusedOnThree = (body: ChangeBody) => (body.operations.length === 3 ? refuse({ op_index: 2, chain: ["authenticated", ROLE] }) : dryOk(SQL));

  it("still_shows_the_exposure_flag_the_dry_run_sql_of_the_changes_that_passed_and_which_passed", async () => {
    await reviewThree(onChanges(refusedOnThree));
    const panel = await screen.findByRole("complementary", { name: "Review changes" });
    // The refusal itself, as before.
    expect(await within(panel).findByRole("alert")).toHaveTextContent(`All rows can't be turned on for ${ROLE}`);
    // The exposure flag.
    const flag = within(panel).getByText(/Exposes data through the data API\./);
    expect(flag.closest("p, div, li")).toHaveTextContent(/authenticated/);
    // The SQL of the two that passed, and which they are.
    for (const line of SQL) expect(within(panel).getByText(line, { exact: false })).toBeInTheDocument();
    expect(within(panel).getByText(/Dry run passed for changes 1 and 2/)).toBeInTheDocument();
    // Apply stays off.
    const apply = within(panel).queryByRole("button", { name: /^Apply/ });
    if (apply) expect(apply).toBeDisabled();
  });

  it("the_second_dry_run_leaves_out_exactly_the_refused_change_and_nothing_is_applied", async () => {
    const m = await reviewThree(onChanges(refusedOnThree));
    await screen.findByText(/Dry run passed for changes 1 and 2/);
    const bodies = await changeBodies(m.f);
    expect(bodies.every((b) => b.dry_run)).toBe(true);
    expect(bodies.map((b) => b.operations.length)).toEqual([3, 2]);
    // The refused change is the third, the all-rows one; the rest keep their order.
    expect(JSON.stringify(bodies[1].operations)).toBe(JSON.stringify(bodies[0].operations.slice(0, 2)));
    expect(bodies[1].expect_acl).toBeDefined();
  });

  it("back_to_editing_keeps_all_three_changes_staged", async () => {
    const m = await reviewThree(onChanges(refusedOnThree));
    await screen.findByText(/Dry run passed for changes 1 and 2/);
    await m.user.click(screen.getByRole("button", { name: /^Back/ }));
    expect(within(pending()).getByRole("heading")).toHaveTextContent("Pending changes · 3");
  });

  it("a_refusal_of_the_only_change_has_no_second_dry_run_and_no_passed_line", async () => {
    const m = await loaded({ routes: onChanges(() => refuse({ op_index: 0, chain: ["anon", ROLE] })) });
    await m.user.click(screen.getByRole("switch", { name: "All rows on refunds" }));
    await m.user.click(within(pending()).getByRole("button", { name: "Review changes" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(`All rows can't be turned on for ${ROLE}`);
    expect(screen.getByText(/Exposes data through the data API\./)).toBeInTheDocument();
    expect(screen.queryByText(/Dry run passed/)).toBeNull();
    expect(await changeBodies(m.f)).toHaveLength(1);
  });

  it("a_second_dry_run_that_fails_too_shows_only_its_own_refusal_and_no_passed_line", async () => {
    await reviewThree(
      onChanges((b) =>
        b.operations.length === 3 ? refuse({ op_index: 2, chain: ["authenticated", ROLE] }) : jsonResponse({ error: { code: "internal", message: "dry run blew up" } }, 500),
      ),
    );
    const panel = await screen.findByRole("complementary", { name: "Review changes" });
    await waitFor(() => expect(within(panel).getByRole("alert")).toBeInTheDocument());
    expect(within(panel).queryByText(/Dry run passed/)).toBeNull();
    const apply = within(panel).queryByRole("button", { name: /^Apply/ });
    if (apply) expect(apply).toBeDisabled();
  });
});

describe("the role page's controls (fix 7)", () => {
  it("the_created_by_select_is_themed", async () => {
    const { container } = await loaded();
    expectThemedSelects(container);
  });

  it("each_new_objects_row_is_a_fixed_grid_so_the_boxes_line_up", async () => {
    await loaded();
    const label = screen.queryByText("New tables");
    if (label) expect(label.closest("li")!.className).toMatch(/\bgrid\b/);
    else expect(document.body).toHaveTextContent(/New tables/);
  });
});
