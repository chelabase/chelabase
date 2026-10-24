import { useState } from "react";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Member, Project } from "@chelabase/management-client";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { InviteDialog } from "./invite-dialog";
import { person } from "./hub/fixtures";

/** Final fix round, area C: S1-m3 (focus after removing a chip) and S1-m4 (Escape on the results). */

const projects = [{ id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] }] as unknown as Project[];
const invited = (email: string): Member => ({ ...person(email.split("@")[0], { status: "invited" }), email });

function setup() {
  const f = fakeFetch({
    "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }),
    "POST /v1/invites": async (request) => {
      const body = await request.json();
      return jsonResponse({ outcome: "invited", member: invited(body.email) }, 201);
    },
  });
  const onDone = vi.fn();
  const onOpenChange = vi.fn();
  function Harness() {
    const [open, setOpen] = useState(true);
    return (
      <InviteDialog
        open={open}
        onOpenChange={(o) => {
          onOpenChange(o);
          setOpen(o);
        }}
        projects={projects}
        onDone={onDone}
        onLink={() => {}}
      />
    );
  }
  renderWithQuery(<Harness />, { fetch: f });
  return { onDone, onOpenChange };
}

describe("InviteDialog: the last edges", () => {
  it("removing_a_chip_returns_focus_to_the_email_field", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    const field = within(dialog).getByLabelText("Emails");
    await userEvent.click(field);
    await userEvent.paste("rui@x.test, nadia@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove rui@x.test" }));
    expect(within(dialog).queryByRole("button", { name: "Remove rui@x.test" })).toBeNull();
    expect(within(dialog).getByRole("button", { name: "Remove nadia@x.test" })).toBeInTheDocument();
    expect(field).toHaveFocus();
  });

  it("removing_the_last_chip_returns_focus_to_the_field_too", async () => {
    setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    const field = within(dialog).getByLabelText("Emails");
    await userEvent.click(field);
    await userEvent.paste("rui@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove rui@x.test" }));
    expect(field).toHaveFocus();
  });

  it("closing_the_results_with_escape_still_reports_what_was_sent", async () => {
    const { onDone, onOpenChange } = setup();
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.click(within(dialog).getByLabelText("Emails"));
    await userEvent.paste("rui@x.test, nadia@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    await screen.findByRole("dialog", { name: "Invites sent: 2 of 2" });
    expect(onDone).not.toHaveBeenCalled();
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    // The same sentence Done would have given.
    expect(onDone).toHaveBeenCalledTimes(1);
    expect(onDone).toHaveBeenCalledWith("Invited 2 people.");
  });

  it("escape_after_a_partial_failure_says_how_many_were_not_sent", async () => {
    const f = fakeFetch({
      "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }),
      "POST /v1/invites": async (request) => {
        const body = await request.json();
        return body.email === "bad@x.test"
          ? jsonResponse({ error: { code: "email_in_use_elsewhere", message: "x" } }, 409)
          : jsonResponse({ outcome: "invited", member: invited(body.email) }, 201);
      },
    });
    const onDone = vi.fn();
    function Harness() {
      const [open, setOpen] = useState(true);
      return <InviteDialog open={open} onOpenChange={setOpen} projects={projects} onDone={onDone} onLink={() => {}} />;
    }
    renderWithQuery(<Harness />, { fetch: f });
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.click(within(dialog).getByLabelText("Emails"));
    await userEvent.paste("rui@x.test, bad@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    await screen.findByRole("dialog", { name: "Invites sent: 1 of 2" });
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(onDone).toHaveBeenCalledTimes(1));
    expect(onDone).toHaveBeenCalledWith("Invited 1 of 2; 1 not sent.");
  });
});
