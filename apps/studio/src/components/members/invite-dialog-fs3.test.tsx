import { useState } from "react";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Project } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { InviteDialog } from "./invite-dialog";

/** Fix S3 (finish review round 2, item 2): the invite compose layout. */

const projects = [{ id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] }] as unknown as Project[];

function setup() {
  const f = fakeFetch({ "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }) });
  function Harness() {
    const [open, setOpen] = useState(true);
    return <InviteDialog open={open} onOpenChange={setOpen} projects={projects} onDone={vi.fn()} onLink={vi.fn()} />;
  }
  renderWithQuery(<Harness />, { fetch: f });
}

describe("invite compose (fix S3)", () => {
  it("delivery_is_one_inline_row_of_radios", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    const group = await within(dialog).findByRole("radiogroup", { name: "How they get the invite" });
    expect(group.className).toMatch(/(^|\s)(flex-row|flex-wrap)(\s|$)/);
    expect(group.className).not.toMatch(/(^|\s)flex-col(\s|$)/);
  });

  it("the_access_heading_says_it_is_the_same_for_everyone", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    // Fix S5: "Access" is bold and the note a muted span inside the legend, so match the legend's whole text.
    expect(dialog.querySelector("legend")).toHaveTextContent("Access · the same for everyone in this invite");
  });

  it("an_access_row_is_one_line_from_sm_up", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    const row = within(dialog).getByRole("group", { name: "Shop" });
    const select = row.querySelector("select")!;
    const segmented = within(row).getAllByRole("radio")[0].closest('[role="radiogroup"]')!;
    const remove = within(row).getByRole("button", { name: "Remove Shop" });
    // One container, laid out as a row from sm, holds the project select, the control and the remove button.
    let box: HTMLElement | null = select.parentElement;
    while (box && box !== row && !(box.contains(segmented) && box.contains(remove))) box = box.parentElement;
    expect(box).not.toBe(row);
    expect(box!.className).toMatch(/sm:(flex-row|grid-cols|flex)\b/);
  });

  it("add_a_project_is_a_ghost_button", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    const add = within(dialog).getByRole("button", { name: "Add a project" });
    expect(add.className).not.toContain("border-border");
    expect(add.className).toContain("hover:bg-muted");
  });
});
