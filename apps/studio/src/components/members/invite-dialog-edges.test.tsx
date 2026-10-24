import { useState } from "react";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Project } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { InviteDialog } from "./invite-dialog";

const projects = [{ id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] }] as unknown as Project[];

function setup() {
  const f = fakeFetch({ "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }) });
  function Harness() {
    const [open, setOpen] = useState(true);
    return <InviteDialog open={open} onOpenChange={setOpen} projects={projects} onDone={() => setOpen(false)} onLink={() => {}} />;
  }
  renderWithQuery(<Harness />, { fetch: f });
}
const chipNames = (dialog: HTMLElement) =>
  within(dialog).queryAllByRole("button", { name: /^Remove \S+@/ }).map((b) => b.getAttribute("aria-label")!.replace(/^Remove /, ""));

describe("invite dialog edges", () => {
  it("Enter closes an address without adding a new line, and Backspace on an empty field takes the last chip back", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    const field = within(dialog).getByLabelText("Emails");
    await userEvent.type(field, "a@x.test{Enter}b@x.test{Enter}");
    expect(chipNames(dialog)).toEqual(["a@x.test", "b@x.test"]);
    expect(field).toHaveValue("");
    await userEvent.type(field, "{Backspace}");
    expect(chipNames(dialog)).toEqual(["a@x.test"]);
  });

  it("one address that is not an email is refused in place and what was typed stays", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    const field = within(dialog).getByLabelText("Emails");
    await userEvent.type(field, "nobody");
    await userEvent.click(await within(dialog).findByRole("button", { name: "Send invite" }));
    expect(within(dialog).getByText("Enter an email address.")).toBeInTheDocument();
    expect(field).toHaveValue("nobody");
  });

  it("an empty field says so and the same address twice is one chip", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.click(await within(dialog).findByRole("button", { name: "Send invite" }));
    expect(within(dialog).getByText("Enter an email address.")).toBeInTheDocument();
    await userEvent.type(within(dialog).getByLabelText("Emails"), "a@x.test, A@X.test ");
    expect(chipNames(dialog)).toEqual(["a@x.test"]);
  });
});
