import { useState } from "react";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Member, Project } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { InviteDialog } from "./invite-dialog";
import { person } from "./hub/fixtures";

const projects = [{ id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] }] as unknown as Project[];
const invited = (email: string): Member => ({ ...person(email.split("@")[0], { status: "invited" }), email });

/** Answers each POST /v1/invites by the address in its body, recording order and concurrency. */
function inviteRoute(by: (email: string, n: number) => Response | Promise<Response>) {
  const log = { emails: [] as string[], bodies: [] as Record<string, unknown>[], inFlight: 0, maxInFlight: 0 };
  const handler: FakeHandler = async (request) => {
    const body = await request.json();
    log.bodies.push(body);
    log.emails.push(body.email);
    log.inFlight++;
    log.maxInFlight = Math.max(log.maxInFlight, log.inFlight);
    try {
      await new Promise((r) => setTimeout(r, 5));
      return await by(body.email, log.emails.length);
    } finally {
      log.inFlight--;
    }
  };
  return { log, handler };
}
const okInvite = (email: string, extra: Record<string, unknown> = {}) =>
  jsonResponse({ outcome: "invited", member: invited(email), ...extra }, 201);

function setup(routes: Record<string, FakeHandler> = {}) {
  const f = fakeFetch({ "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }), ...routes });
  const onOpenChange = vi.fn();
  function Harness() {
    const [open, setOpen] = useState(false);
    return (
      <>
        <button onClick={() => setOpen(true)}>Open invite</button>
        <InviteDialog
          open={open}
          onOpenChange={(o) => {
            onOpenChange(o);
            setOpen(o);
          }}
          projects={projects}
          onDone={() => setOpen(false)}
          onLink={() => {}}
        />
      </>
    );
  }
  renderWithQuery(<Harness />, { fetch: f });
  // The page behind a modal is aria-hidden once it opens: take the trigger first.
  const trigger = screen.getByRole("button", { name: "Open invite" });
  return { f, onOpenChange, trigger };
}

async function open(trigger: HTMLElement) {
  await userEvent.click(trigger);
  const dialog = await screen.findByRole("dialog", { name: "Invite people" });
  return { dialog, field: within(dialog).getByLabelText("Emails") };
}
const posts = (f: { calls: Request[] }) => f.calls.filter((r) => r.method === "POST" && new URL(r.url).pathname === "/v1/invites");
const chipNames = (dialog: HTMLElement) =>
  within(dialog)
    .queryAllByRole("button", { name: /^Remove \S+@/ })
    .map((b) => b.getAttribute("aria-label")!.replace(/^Remove /, ""));

describe("multi-email invite", () => {
  it("turns commas, spaces and new lines into chips and flags invalid ones", async () => {
    const { trigger } = setup();
    const { dialog, field } = await open(trigger);
    await userEvent.click(field);
    await userEvent.paste("rui@x.test, nadia@x.test\nben@northwind  ines@x.test");
    expect(chipNames(dialog)).toEqual(["rui@x.test", "nadia@x.test", "ben@northwind", "ines@x.test"]);
    // Typing works the same: a separator closes the chip.
    await userEvent.type(field, "last@x.test,");
    expect(chipNames(dialog)).toContain("last@x.test");
    // An address can be taken back out.
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove rui@x.test" }));
    expect(chipNames(dialog)).not.toContain("rui@x.test");
  });

  it("invalid_emails_are_flagged_per_chip_and_not_sent", async () => {
    const route = inviteRoute((email) => okInvite(email));
    const { f, trigger } = setup({ "POST /v1/invites": route.handler });
    const { dialog, field } = await open(trigger);
    await userEvent.click(field);
    await userEvent.paste("rui@x.test, ben@northwind, Nadia@X.test, rui@x.test");
    // The bad chip is flagged where it is, with its reason in words; the duplicate is one chip.
    expect(within(dialog).getByText(/ben@northwind isn't an email address/)).toBeInTheDocument();
    expect(chipNames(dialog).filter((c) => c === "rui@x.test")).toHaveLength(1);
    expect(within(dialog).getByText(/2 invites/)).toBeInTheDocument();
    expect(within(dialog).getByText(/1 .*to fix/)).toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    await waitFor(() => expect(route.log.emails).toHaveLength(2));
    expect(route.log.emails).toEqual(["rui@x.test", "nadia@x.test"]);
    expect(posts(f)).toHaveLength(2);
    expect(route.log.emails).not.toContain("ben@northwind");
  });

  it("multi_email_invite_sends_one_request_per_email_and_shows_each_result", async () => {
    const route = inviteRoute((email) => {
      if (email === "ops@contractor.io") {
        return jsonResponse({ error: { code: "signup_pending", message: "x", member_id: "m-ops" } }, 409);
      }
      if (email === "ines@x.test") return jsonResponse({ outcome: "existing_member", member: invited(email) }, 200);
      return okInvite(email);
    });
    const { f, trigger, onOpenChange } = setup({ "POST /v1/invites": route.handler });
    const { dialog, field } = await open(trigger);
    await userEvent.click(field);
    await userEvent.paste("rui@x.test\nops@contractor.io\nines@x.test\nnadia@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    await userEvent.click(within(dialog).getByRole("radio", { name: /Viewer/ }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 4 invites" }));
    expect(await screen.findByRole("dialog", { name: "Invites sent: 2 of 4" })).toBeInTheDocument();

    // One request per address, in order, one at a time, the same access in each; the failure doesn't stop the rest.
    expect(route.log.emails).toEqual(["rui@x.test", "ops@contractor.io", "ines@x.test", "nadia@x.test"]);
    expect(route.log.maxInFlight).toBe(1);
    expect(posts(f)).toHaveLength(4);
    for (const body of route.log.bodies) {
      expect(body).toEqual({ email: body.email, delivery: "email", grants: [{ project_id: "p1", platform_role: "viewer", pg_roles: [] }] });
    }

    const results = within(screen.getByRole("dialog")).getAllByRole("listitem");
    expect(results).toHaveLength(4);
    expect(results[0]).toHaveTextContent(/rui@x\.test/);
    expect(results[0]).toHaveTextContent(/invited/i);
    expect(results[1]).toHaveTextContent(/ops@contractor\.io/);
    expect(results[1]).toHaveTextContent(/not sent/i);
    expect(results[1]).toHaveTextContent(/ops@contractor\.io signed up and is waiting for approval/);
    expect(results[2]).toHaveTextContent(/already a member/i);
    expect(results[3]).toHaveTextContent(/nadia@x\.test/);

    // Retry only the one that failed.
    const retry = screen.getByRole("button", { name: /retry the failed/i });
    await userEvent.click(retry);
    const again = await screen.findByRole("dialog", { name: "Invite people" });
    expect(chipNames(again)).toEqual(["ops@contractor.io"]);
    expect(onOpenChange).not.toHaveBeenCalledWith(false);
  });

  it("closes from Done", async () => {
    const route = inviteRoute((email) => okInvite(email));
    const { trigger, onOpenChange } = setup({ "POST /v1/invites": route.handler });
    const { dialog, field } = await open(trigger);
    await userEvent.click(field);
    await userEvent.paste("rui@x.test, nadia@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    await screen.findByRole("dialog", { name: "Invites sent: 2 of 2" });
    await userEvent.click(screen.getByRole("button", { name: "Done" }));
    expect(onOpenChange).toHaveBeenLastCalledWith(false);
  });

  it("refuses more than 50 addresses and sends none of them", async () => {
    const route = inviteRoute((email) => okInvite(email));
    const { f, trigger } = setup({ "POST /v1/invites": route.handler });
    const { dialog, field } = await open(trigger);
    await userEvent.click(field);
    const many = Array.from({ length: 51 }, (_, i) => `user${i}@x.test`).join(", ");
    await userEvent.paste(many);
    expect(within(dialog).getByRole("alert")).toHaveTextContent(/50/);
    const send = within(dialog).queryByRole("button", { name: /^Send \d+ invites$/ });
    if (send && !(send as HTMLButtonElement).disabled) await userEvent.click(send);
    expect(posts(f)).toHaveLength(0);
  });

  it("accepts exactly 50 addresses and sends them one at a time", async () => {
    const route = inviteRoute((email) => okInvite(email));
    const { trigger } = setup({ "POST /v1/invites": route.handler });
    const { dialog, field } = await open(trigger);
    await userEvent.click(field);
    await userEvent.paste(Array.from({ length: 50 }, (_, i) => `user${i}@x.test`).join(" "));
    expect(within(dialog).queryByRole("alert")).toBeNull();
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 50 invites" }));
    await screen.findByRole("dialog", { name: "Invites sent: 50 of 50" }, { timeout: 15_000 });
    expect(route.log.maxInFlight).toBe(1);
    expect(route.log.emails).toHaveLength(50);
  }, 30_000);

  it("lists link invites together, each URL once, and drops them when the dialog closes", async () => {
    const route = inviteRoute((email) => okInvite(email, { invite_url: `https://studio.test/invite#${email.split("@")[0]}-secret` }));
    const { f, trigger } = setup({ "POST /v1/invites": route.handler });
    const { dialog, field } = await open(trigger);
    await userEvent.click(field);
    await userEvent.paste("rui@x.test, nadia@x.test, ines@x.test");
    await userEvent.click(within(dialog).getByRole("radio", { name: /One-time link/ }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 3 invites" }));
    const results = await screen.findByRole("dialog", { name: "Invites sent: 3 of 3" });
    expect(route.log.bodies.every((b) => b.delivery === "link")).toBe(true);
    for (const name of ["rui", "nadia", "ines"]) {
      expect(within(results).getAllByDisplayValue(`https://studio.test/invite#${name}-secret`)).toHaveLength(1);
    }
    expect(within(results).getAllByRole("button", { name: /^Copy/ })).toHaveLength(3);
    expect(posts(f)).toHaveLength(3);

    await userEvent.click(within(results).getByRole("button", { name: "Done" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    // Reopened: no leftover results or links.
    await userEvent.click(screen.getByRole("button", { name: "Open invite" }));
    const again = await screen.findByRole("dialog", { name: "Invite people" });
    expect(within(again).queryByDisplayValue(/secret/)).toBeNull();
    expect(chipNames(again)).toEqual([]);
  });
});
