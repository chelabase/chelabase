import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { renderWithQuery } from "@/test/render";
import { AppHeader } from "./app-header";

describe("AppHeader", () => {
  it("shows the name in the trigger", () => {
    renderWithQuery(<AppHeader profile={{ name: "Ada", email: "ada@x.dev" }} onSignOut={() => {}} />);
    expect(screen.getByRole("button", { name: "Ada" })).toBeInTheDocument();
  });

  it("falls back to the email when there is no name", () => {
    renderWithQuery(<AppHeader profile={{ email: "ada@x.dev" }} onSignOut={() => {}} />);
    expect(screen.getByRole("button", { name: "ada@x.dev" })).toBeInTheDocument();
  });

  it("falls back to the email when the name is empty", () => {
    renderWithQuery(<AppHeader profile={{ name: "", email: "ada@x.dev" }} onSignOut={() => {}} />);
    expect(screen.getByRole("button", { name: "ada@x.dev" })).toBeInTheDocument();
  });

  it("Sign out still clears the cache and signs out with neither name nor email", async () => {
    const user = userEvent.setup();
    const onSignOut = vi.fn();
    const { queryClient } = renderWithQuery(<AppHeader profile={{}} onSignOut={onSignOut} />);
    const clear = vi.spyOn(queryClient, "clear");
    await user.click(screen.getByRole("button", { name: "Account" }));
    await user.click(await screen.findByRole("menuitem", { name: "Sign out" }));
    expect(clear).toHaveBeenCalledTimes(1);
    expect(onSignOut).toHaveBeenCalledTimes(1);
  });

  it("names the trigger Account when there is neither", () => {
    renderWithQuery(<AppHeader profile={{}} onSignOut={() => {}} />);
    expect(screen.getByRole("button", { name: "Account" })).toBeInTheDocument();
  });

  it("shows the email in the menu", async () => {
    const user = userEvent.setup();
    renderWithQuery(<AppHeader profile={{ name: "Ada", email: "ada@x.dev" }} onSignOut={() => {}} />);
    await user.click(screen.getByRole("button", { name: "Ada" }));
    expect(await screen.findByText("ada@x.dev")).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: "Sign out" })).toBeInTheDocument();
  });

  it("Sign out clears the query cache before calling onSignOut", async () => {
    const user = userEvent.setup();
    let cachedAtCall = -1;
    const holder: { qc?: ReturnType<typeof renderWithQuery>["queryClient"] } = {};
    const onSignOut = vi.fn(() => {
      cachedAtCall = holder.qc?.getQueryCache().getAll().length ?? -1;
    });
    const { queryClient } = renderWithQuery(<AppHeader profile={{ name: "Ada" }} onSignOut={onSignOut} />);
    holder.qc = queryClient;
    queryClient.setQueryData(["secret"], { token: "x" });
    const clear = vi.spyOn(queryClient, "clear");
    expect(queryClient.getQueryCache().getAll().length).toBe(1);

    await user.click(screen.getByRole("button", { name: "Ada" }));
    await user.click(await screen.findByRole("menuitem", { name: "Sign out" }));

    expect(clear).toHaveBeenCalledTimes(1);
    expect(onSignOut).toHaveBeenCalledTimes(1);
    expect(cachedAtCall).toBe(0);
    expect(clear.mock.invocationCallOrder[0]).toBeLessThan(onSignOut.mock.invocationCallOrder[0]);
  });
});
