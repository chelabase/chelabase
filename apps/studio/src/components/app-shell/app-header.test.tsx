import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { renderWithRouter } from "@/test/render-router";
import { AppHeader } from "./app-header";

describe("AppHeader", () => {
  it("marks Studio as alpha, not for production data", async () => {
    renderWithRouter(<AppHeader profile={{ name: "Ada" }} onSignOut={() => {}} />);
    const tag = await screen.findByText("Alpha");
    expect(tag.closest("[title]")).toHaveAttribute("title", expect.stringMatching(/not ready for production data/));
    expect(screen.getByText(/not ready for production data/, { selector: ".sr-only" })).toBeInTheDocument();
  });

  it("shows the name in the trigger", async () => {
    renderWithRouter(<AppHeader profile={{ name: "Ada", email: "ada@x.dev" }} onSignOut={() => {}} />);
    expect(await screen.findByRole("button", { name: "Ada" })).toBeInTheDocument();
  });

  it("falls back to the email when there is no name", async () => {
    renderWithRouter(<AppHeader profile={{ email: "ada@x.dev" }} onSignOut={() => {}} />);
    expect(await screen.findByRole("button", { name: "ada@x.dev" })).toBeInTheDocument();
  });

  it("falls back to the email when the name is empty", async () => {
    renderWithRouter(<AppHeader profile={{ name: "", email: "ada@x.dev" }} onSignOut={() => {}} />);
    expect(await screen.findByRole("button", { name: "ada@x.dev" })).toBeInTheDocument();
  });

  it("Sign out still clears the cache and signs out with neither name nor email", async () => {
    const user = userEvent.setup();
    const onSignOut = vi.fn();
    const { queryClient } = renderWithRouter(<AppHeader profile={{}} onSignOut={onSignOut} />);
    const clear = vi.spyOn(queryClient, "clear");
    await user.click(await screen.findByRole("button", { name: "Account" }));
    await user.click(await screen.findByRole("menuitem", { name: "Sign out" }));
    expect(clear).toHaveBeenCalledTimes(1);
    expect(onSignOut).toHaveBeenCalledTimes(1);
  });

  it("names the trigger Account when there is neither", async () => {
    renderWithRouter(<AppHeader profile={{}} onSignOut={() => {}} />);
    expect(await screen.findByRole("button", { name: "Account" })).toBeInTheDocument();
  });

  it("shows the email in the menu", async () => {
    const user = userEvent.setup();
    renderWithRouter(<AppHeader profile={{ name: "Ada", email: "ada@x.dev" }} onSignOut={() => {}} />);
    await user.click(await screen.findByRole("button", { name: "Ada" }));
    expect(await screen.findByText("ada@x.dev")).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Sign out" })).toBeInTheDocument();
  });

  it("Sign out clears the query cache before calling onSignOut", async () => {
    const user = userEvent.setup();
    let cachedAtCall = -1;
    const holder: { qc?: ReturnType<typeof renderWithRouter>["queryClient"] } = {};
    const onSignOut = vi.fn(() => {
      cachedAtCall = holder.qc?.getQueryCache().getAll().length ?? -1;
    });
    const { queryClient } = renderWithRouter(<AppHeader profile={{ name: "Ada" }} onSignOut={onSignOut} />);
    holder.qc = queryClient;
    queryClient.setQueryData(["secret"], { token: "x" });
    const clear = vi.spyOn(queryClient, "clear");
    expect(queryClient.getQueryCache().getAll().length).toBe(1);

    await user.click(await screen.findByRole("button", { name: "Ada" }));
    await user.click(await screen.findByRole("menuitem", { name: "Sign out" }));

    expect(clear).toHaveBeenCalledTimes(1);
    expect(onSignOut).toHaveBeenCalledTimes(1);
    expect(cachedAtCall).toBe(0);
    expect(clear.mock.invocationCallOrder[0]).toBeLessThan(onSignOut.mock.invocationCallOrder[0]);
  });

  it("shows no menu button unless given onMenu", async () => {
    renderWithRouter(<AppHeader profile={{ name: "Ada" }} onSignOut={() => {}} />);
    await screen.findByRole("button", { name: "Ada" });
    expect(screen.queryByRole("button", { name: "Menu" })).toBeNull();
  });

  it("the menu button comes first and calls onMenu", async () => {
    const user = userEvent.setup();
    const onMenu = vi.fn();
    renderWithRouter(<AppHeader profile={{ name: "Ada" }} onSignOut={() => {}} onMenu={onMenu} menuControlsId="d1" />);
    const button = await screen.findByRole("button", { name: "Menu" });
    expect(screen.getAllByRole("button")[0]).toBe(button);
    expect(button).toHaveAttribute("aria-expanded", "false");
    expect(button).not.toHaveAttribute("aria-controls");
    await user.click(button);
    expect(onMenu).toHaveBeenCalledTimes(1);
  });
});
