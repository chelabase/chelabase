import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { member } from "@/test/fake-realtime";
import { PresenceLayer, type PresenceGeometry } from "./presence-layer";

const key = (id: string) => JSON.stringify([id]);
const KEYS = [key("r1"), key("r2"), key("r3")];
const geometry: PresenceGeometry = {
  rowHeight: 32,
  offsetTop: 40,
  columnBox: (c) => ({ memo: { left: 200, width: 120 }, amount: { left: 80, width: 120 } })[c as "memo"] ?? null,
};
const RECT = { left: 100, top: 50 };
const at = (x: number, y: number) => ({ clientX: RECT.left + x, clientY: RECT.top + y });
const MEMO_R2 = at(260, 40 + 32 + 16);

const ana = member({ id: "ana", sub: "u-ana", name: "Ana Lee", email: "ana@x.test", role: "billing_admin", color_seed: 3, state: { row: key("r2"), column: "memo", editing: false } });
const bo = member({ id: "bo", sub: "u-bo", name: "Bo Kim", email: "bo@x.test", color_seed: 4, state: { row: key("r2"), column: "memo", editing: false } });

function grid(members = [ana], rowKeys = KEYS) {
  const ui = (m: typeof members, r: string[]) => (
    <div role="grid">
      <div role="rowgroup" style={{ position: "relative" }}>
        <PresenceLayer members={m} selfSub="u2" rowKeys={r} geometry={geometry} />
        <div role="gridcell" data-testid="cell" tabIndex={-1} />
      </div>
    </div>
  );
  const view = render(ui(members, rowKeys));
  return { ...view, cell: screen.getByTestId("cell"), again: (m: typeof members, r: string[]) => view.rerender(ui(m, r)) };
}

beforeEach(() => {
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(
    () => ({ ...RECT, right: RECT.left + 1000, bottom: RECT.top + 1000, x: RECT.left, y: RECT.top, width: 1000, height: 1000, toJSON: () => ({}) }) as DOMRect,
  );
});
afterEach(() => vi.restoreAllMocks());

describe("PresenceLayer hover edges", () => {
  it("several_members_on_one_cell_share_one_tag_listing_each", async () => {
    const { cell } = grid([ana, bo]);
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R2 });
    const tags = await screen.findAllByRole("tooltip");
    expect(tags).toHaveLength(1);
    expect(tags[0]).toHaveTextContent("Ana Lee");
    expect(tags[0]).toHaveTextContent("Bo Kim");
  });

  it("the_tag_closes_when_the_hovered_members_row_scrolls_out", async () => {
    const { cell, again } = grid();
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R2 });
    await screen.findByRole("tooltip");
    again([ana], [key("r1"), key("r3")]);
    await waitFor(() => expect(screen.queryByRole("tooltip")).toBeNull());
  });

  it("the_tag_closes_when_the_member_leaves", async () => {
    const { cell, again } = grid();
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R2 });
    await screen.findByRole("tooltip");
    again([], KEYS);
    await waitFor(() => expect(screen.queryByRole("tooltip")).toBeNull());
  });

  it("the_viewers_own_selection_never_opens_a_tag", async () => {
    const mine = member({ id: "m1", sub: "u2", name: "Me", state: { row: key("r2"), column: "memo", editing: false } });
    const { cell } = grid([mine]);
    await userEvent.setup().pointer({ target: cell, coords: MEMO_R2 });
    expect(screen.queryByRole("tooltip")).toBeNull();
  });

  it("a_pointer_above_the_first_row_or_below_the_last_opens_nothing", async () => {
    const { cell } = grid();
    const user = userEvent.setup();
    await user.pointer({ target: cell, coords: at(260, 10) });
    expect(screen.queryByRole("tooltip")).toBeNull();
    await user.pointer({ target: cell, coords: at(260, 40 + 32 * 3 + 5) });
    expect(screen.queryByRole("tooltip")).toBeNull();
  });

  it("the_listeners_are_removed_on_unmount", () => {
    const { container, unmount } = grid();
    const host = container.querySelector('[role="rowgroup"]') as HTMLElement;
    const removed = vi.spyOn(host, "removeEventListener");
    unmount();
    expect(removed.mock.calls.map((c) => c[0])).toEqual(expect.arrayContaining(["pointermove", "mousemove", "pointerleave", "mouseleave"]));
  });

  it("the_layer_stays_aria_hidden", () => {
    grid();
    expect(screen.getByTestId("presence-layer")).toHaveAttribute("aria-hidden", "true");
  });
});
