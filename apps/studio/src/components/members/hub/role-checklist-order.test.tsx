import { useState } from "react";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { RoleChoice } from "../role-picker";
import { RoleChecklist } from "./index";

/** S1-m2: the checklist sorts checked roles first when it mounts or the filter changes, never while a box is clicked. */

const choices: RoleChoice[] = Array.from({ length: 10 }, (_, i) => ({ value: `role_${String(i).padStart(2, "0")}`, chain: [] }));

function Stateful({ start }: { start: string[] }) {
  const [checked, setChecked] = useState(start);
  return <RoleChecklist choices={choices} checked={checked} onChange={setChecked} />;
}

const order = () => screen.getAllByRole("checkbox").map((c) => c.getAttribute("aria-label"));
const names = (...n: number[]) => n.map((i) => `role_${String(i).padStart(2, "0")}`);
const rest = (...skip: number[]) => names(...[0, 1, 2, 3, 4, 5, 6, 7, 8, 9].filter((i) => !skip.includes(i)));

describe("RoleChecklist: a stable order while clicking (S1-m2)", () => {
  it("checking_a_role_leaves_every_row_where_it_was", async () => {
    render(<Stateful start={names(7)} />);
    // Checked first at mount.
    expect(order()).toEqual([...names(7), ...rest(7)]);
    await userEvent.click(screen.getByRole("checkbox", { name: "role_02" }));
    expect(screen.getByRole("checkbox", { name: "role_02" })).toBeChecked();
    // The row under the pointer did not jump.
    expect(order()).toEqual([...names(7), ...rest(7)]);
  });

  it("unchecking_a_role_leaves_every_row_where_it_was", async () => {
    render(<Stateful start={names(7, 3)} />);
    expect(order()).toEqual([...names(3, 7), ...rest(3, 7)]);
    await userEvent.click(screen.getByRole("checkbox", { name: "role_03" }));
    expect(screen.getByRole("checkbox", { name: "role_03" })).not.toBeChecked();
    expect(order()).toEqual([...names(3, 7), ...rest(3, 7)]);
  });

  it("a_filter_change_sorts_again_and_so_does_reopening", async () => {
    const view = render(<Stateful start={names(7)} />);
    await userEvent.click(screen.getByRole("checkbox", { name: "role_02" }));
    const filter = screen.getByRole("searchbox");
    await userEvent.type(filter, "role_0");
    await userEvent.clear(filter);
    // Checked roles are first again (in the choices' order), then the rest.
    expect(order()).toEqual([...names(2, 7), ...rest(2, 7)]);
    // A fresh mount (the card reopened) sorts from the current checked set.
    view.unmount();
    render(<Stateful start={names(9, 5)} />);
    expect(order()).toEqual([...names(5, 9), ...rest(5, 9)]);
  });
});
