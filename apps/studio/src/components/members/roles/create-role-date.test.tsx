import { screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import { mountTab } from "./test-setup";

/** Fix S (coder's own): the typed "Valid until" refuses days that don't exist and accepts a bare date. */
beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

describe("create role: Valid until", () => {
  it("refuses_a_day_that_does_not_exist_and_accepts_a_date_alone", async () => {
    const m = mountTab();
    await screen.findByRole("link", { name: /^billing_editor\b/ });
    await m.user.click(screen.getByRole("button", { name: "Create role" }));
    const dialog = await screen.findByRole("dialog", { name: "Create a role" });
    await m.user.click(within(dialog).getByRole("button", { name: "Advanced" }));
    const field = within(dialog).getByLabelText("Valid until");
    await m.user.type(field, "2030-02-31 10:00");
    expect(within(dialog).getByText("Pick a date and time, or leave it empty.")).toBeInTheDocument();
    await m.user.clear(field);
    await m.user.type(field, "2030-02-28");
    expect(within(dialog).queryByText("Pick a date and time, or leave it empty.")).toBeNull();
  });
});
