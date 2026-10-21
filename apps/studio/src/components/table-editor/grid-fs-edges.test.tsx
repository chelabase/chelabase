import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { stubLayout } from "@/test/table-editor-fixtures";
import { Grid, type GridProps } from "./grid";

/** RT1 Playwright pass, F1/F2 edges beyond the RED set (coder's own). */

type Row = Record<string, unknown>;
const ROWS: Row[] = [
  { id: "r1", n: "10", memo: "m1" },
  { id: "r2", n: "20", memo: "m2" },
  { id: "r3", n: "30", memo: "m3" },
];
const KEYS = ['["r1"]', '["r2"]', '["r3"]'];
const columns = ["id", "n", "memo"].map((name) => ({
  name,
  desc: { type: "text", nullable: true, has_default: false, generated: false, array: false, enum: null },
  labels: null,
  readOnly: false,
}));

function props(over: Partial<GridProps> = {}): GridProps {
  return {
    label: "public.t", columns, rows: ROWS, firstRowNumber: 1, keys: KEYS,
    isSelected: () => false, pageSelection: "none", onToggleRow: () => {}, onTogglePage: () => {}, sort: null, onSort: vi.fn(),
    canEdit: () => true, onSave: async () => null, onCopy: () => {}, widths: {}, onWidths: () => {}, onPresence: vi.fn(),
    ...over,
  } as unknown as GridProps;
}
function setup(over: Partial<GridProps> = {}) {
  const p = props(over);
  const view = render(
    <div>
      <button type="button">toolbar</button>
      <Grid {...p} />
    </div>,
  );
  return { ...view, props: p, user: userEvent.setup() };
}
const grid = () => screen.getByRole("grid", { name: "public.t" });
const cell = (name: string) => within(grid()).getByRole("gridcell", { name });
const field = (name: string) => screen.queryByRole("textbox", { name });
const activeCells = () => Array.from(document.querySelectorAll('[data-active="true"]'));
const deferred = <T,>() => {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => (resolve = r));
  return { promise, resolve };
};

beforeEach(() => stubLayout());

describe("F1/F2 edges", () => {
  it("a_press_on_another_cell_while_an_enter_save_is_in_flight_lands_there_once_the_save_succeeds", async () => {
    const save = deferred<string | null>();
    const onSave = vi.fn(() => save.promise);
    const { user } = setup({ onSave });
    await user.dblClick(cell("20"));
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9{Enter}");
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    const pressed = cell("m3");
    await user.click(pressed);
    save.resolve(null);
    await waitFor(() => expect(field("n")).toBeNull());
    expect(activeCells()).toEqual([pressed]);
    expect(document.activeElement).toBe(pressed);
    expect(onSave).toHaveBeenCalledTimes(1);
  });

  it("after_a_refused_save_a_press_with_the_same_value_saves_nothing_more_and_keeps_the_editor", async () => {
    const onSave = vi.fn(async () => "Not a number");
    const { user } = setup({ onSave });
    const edited = cell("20");
    await user.dblClick(edited);
    await user.type(await screen.findByRole("textbox", { name: "n" }), "x");
    await user.click(cell("m3"));
    await screen.findByRole("alert");
    await user.click(cell("30"));
    expect(onSave).toHaveBeenCalledTimes(1);
    expect(field("n")).toBeInTheDocument();
    expect(activeCells()).toEqual([edited]);
    expect(document.activeElement).toBe(field("n"));
  });

  it("a_retry_after_a_refused_save_then_a_press_moves_on_once_it_saves", async () => {
    const onSave = vi.fn().mockResolvedValueOnce("Not a number").mockResolvedValueOnce(null);
    const { user } = setup({ onSave });
    await user.dblClick(cell("20"));
    const input = await screen.findByRole("textbox", { name: "n" });
    await user.type(input, "x");
    await user.click(cell("m3"));
    await screen.findByRole("alert");
    await user.type(input, "{Backspace}5");
    const pressed = cell("m3");
    await user.click(pressed);
    await waitFor(() => expect(field("n")).toBeNull());
    expect(onSave).toHaveBeenCalledTimes(2);
    expect(onSave).toHaveBeenLastCalledWith('["r2"]', expect.objectContaining({ name: "n" }), { null: false, text: "205" });
    expect(activeCells()).toEqual([pressed]);
  });

  it("a_save_that_lands_after_the_view_changed_moves_nothing_and_opens_nothing", async () => {
    const save = deferred<string | null>();
    const onSave = vi.fn(() => save.promise);
    const { user, rerender, props: p } = setup({ onSave, resetKey: "a" });
    await user.dblClick(cell("20"));
    await user.type(await screen.findByRole("textbox", { name: "n" }), "9");
    await user.dblClick(cell("m3"));
    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    rerender(
      <div>
        <button type="button">toolbar</button>
        <Grid {...p} resetKey="b" />
      </div>,
    );
    await waitFor(() => expect(field("n")).toBeNull());
    save.resolve(null);
    await new Promise((r) => setTimeout(r, 20));
    expect(field("memo")).toBeNull();
    expect(activeCells()).toEqual([]);
  });

  it("a_right_press_on_another_cell_still_just_selects_it_for_its_menu", async () => {
    const onSave = vi.fn(async () => null);
    const { user } = setup({ onSave });
    await user.dblClick(cell("20"));
    await screen.findByRole("textbox", { name: "n" });
    const pressed = cell("m3");
    await user.pointer({ keys: "[MouseRight]", target: pressed });
    expect(activeCells()).toEqual([pressed]);
    expect(onSave).not.toHaveBeenCalled();
  });
});
