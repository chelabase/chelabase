import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { RailBehaviourMenu } from "./rail-behaviour-menu";

function setup(props: Partial<Parameters<typeof RailBehaviourMenu>[0]> = {}) {
  const onBehaviourChange = vi.fn();
  const onOpenChange = vi.fn();
  render(
    <RailBehaviourMenu
      behaviour="hover"
      onBehaviourChange={onBehaviourChange}
      onOpenChange={onOpenChange}
      labelsVisible
      {...props}
    />,
  );
  return { onBehaviourChange, onOpenChange, user: userEvent.setup() };
}

describe("RailBehaviourMenu", () => {
  it("trigger is named Sidebar behaviour", () => {
    setup({ labelsVisible: false });
    expect(screen.getByRole("button", { name: "Sidebar behaviour" })).toBeInTheDocument();
    expect(screen.queryByText("Sidebar")).not.toBeInTheDocument();
  });

  it("label hidden: focusing the trigger shows a Sidebar behaviour tooltip", async () => {
    const { user } = setup({ labelsVisible: false });
    await user.tab();
    expect(screen.getByRole("button", { name: "Sidebar behaviour" })).toHaveFocus();
    await waitFor(() => expect(screen.getByText("Sidebar behaviour")).toBeInTheDocument());
  });

  it("label hidden: hovering the trigger shows the tooltip", async () => {
    const { user } = setup({ labelsVisible: false });
    await user.hover(screen.getByRole("button", { name: "Sidebar behaviour" }));
    await waitFor(() => expect(screen.getByText("Sidebar behaviour")).toBeInTheDocument());
  });

  it("label visible: no tooltip appears", async () => {
    const { user } = setup({ labelsVisible: true });
    await user.tab();
    await new Promise((resolve) => setTimeout(resolve, 450));
    expect(screen.queryByText("Sidebar behaviour")).not.toBeInTheDocument();
  });

  it("menu open: no tooltip while the menu is open", async () => {
    const { user } = setup({ labelsVisible: false });
    await user.click(screen.getByRole("button", { name: "Sidebar behaviour" }));
    await screen.findByRole("menu");
    await new Promise((resolve) => setTimeout(resolve, 450));
    expect(screen.queryByText("Sidebar behaviour")).not.toBeInTheDocument();
  });

  it("shows the three behaviours with the current one checked", async () => {
    const { user } = setup();
    await user.click(screen.getByRole("button", { name: "Sidebar behaviour" }));
    const hover = await screen.findByRole("menuitemradio", { name: "Expand on hover" });
    expect(hover).toHaveAttribute("aria-checked", "true");
    expect(screen.getByRole("menuitemradio", { name: "Expanded" })).toHaveAttribute("aria-checked", "false");
    expect(screen.getByRole("menuitemradio", { name: "Collapsed" })).toHaveAttribute("aria-checked", "false");
  });

  it("picking one calls onBehaviourChange", async () => {
    const { user, onBehaviourChange } = setup();
    await user.click(screen.getByRole("button", { name: "Sidebar behaviour" }));
    await user.click(await screen.findByRole("menuitemradio", { name: "Collapsed" }));
    expect(onBehaviourChange).toHaveBeenCalledWith("collapsed");
  });

  it("picking a behaviour closes the menu", async () => {
    const { user } = setup();
    await user.click(screen.getByRole("button", { name: "Sidebar behaviour" }));
    await user.click(await screen.findByRole("menuitemradio", { name: "Collapsed" }));
    await waitFor(() => expect(screen.queryByRole("menu")).not.toBeInTheDocument());
  });

  it("picking with the keyboard (Enter) closes the menu", async () => {
    const { user, onBehaviourChange } = setup();
    await user.click(screen.getByRole("button", { name: "Sidebar behaviour" }));
    await screen.findByRole("menu");
    await user.keyboard("{ArrowDown}{Enter}");
    expect(onBehaviourChange).toHaveBeenCalled();
    await waitFor(() => expect(screen.queryByRole("menu")).not.toBeInTheDocument());
  });

  it("reports open and close through onOpenChange", async () => {
    const { user, onOpenChange } = setup();
    await user.click(screen.getByRole("button", { name: "Sidebar behaviour" }));
    await screen.findByRole("menu");
    expect(onOpenChange).toHaveBeenLastCalledWith(true);
    await user.keyboard("{Escape}");
    expect(onOpenChange).toHaveBeenLastCalledWith(false);
  });
});
