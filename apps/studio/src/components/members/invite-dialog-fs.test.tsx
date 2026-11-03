import { useState } from "react";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Member, Project } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { expectThemedSelects } from "@/test/themed";
import { InviteDialog } from "./invite-dialog";
import { person } from "./hub/fixtures";

/** Fix S: the invite dialog (finish-review fixes 1 and 6, BP-1, BP-2). */

const projects = [
  { id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] },
  {
    id: "p2", slug: "blog", name: "Blog", status: "active",
    databases: [{ database_id: "d2", name: "posts", postgres_database: "posts", status: "ready", host: null, port: null, branch_count: 1 }],
  },
] as unknown as Project[];

const EXPIRES = "2099-01-01T00:00:00Z";
const invited = (email: string, extra: Partial<Member> = {}): Member => ({ ...person(email.split("@")[0], { status: "invited" }), email, invite: { id: `inv-${email.split("@")[0]}`, expires_at: EXPIRES }, ...extra });
const active = (email: string): Member => ({ ...person(email.split("@")[0], { level: "viewer" }), email });

const role = (name: string) => ({
  name, origin: "sql", level: "plain", login: false, inherit: true, bypass_rls: false, member_of: [], member_admin: [], member_no_set: [],
  blocked_keys: [], connection_limit: -1, created_at: "2026-10-01T00:00:00Z", password: "none", pending: false, guard_exempt: false,
});

/** Answers POST /v1/invites by address; `answers` maps an address to its response. */
function setup(answers: Record<string, () => Response>, extra: Record<string, FakeHandler> = {}, delivery = { email_available: true, reason: null as string | null }) {
  const f = fakeFetch({
    "GET /v1/invites/delivery": () => jsonResponse(delivery),
    "POST /v1/invites": async (request) => {
      const body = await request.json();
      return (answers[body.email] ?? (() => jsonResponse({ outcome: "invited", member: invited(body.email) }, 201)))();
    },
    "GET /v1/databases/d2/branches": () => jsonResponse([{ id: "b2", parent_branch_id: null, name: "main", status: "ready", kind: "main" }]),
    "GET /v1/databases/d2/branches/b2/roles": () => jsonResponse([role("posts_reader"), role("posts_writer")]),
    ...extra,
  });
  const onDone = vi.fn();
  const onLink = vi.fn();
  function Harness() {
    const [open, setOpen] = useState(true);
    return <InviteDialog open={open} onOpenChange={setOpen} projects={projects} onDone={onDone} onLink={onLink} />;
  }
  const view = renderWithQuery(<Harness />, { fetch: f });
  return { f, onDone, onLink, ...view };
}

const post = (f: { calls: Request[] }, path: string) => f.calls.filter((r) => r.method === "POST" && new URL(r.url).pathname === path);
async function compose(emails: string) {
  const dialog = await screen.findByRole("dialog", { name: "Invite people" });
  await userEvent.click(within(dialog).getByLabelText("Emails"));
  await userEvent.paste(emails);
  return dialog;
}
const resend = (id: string, url?: string): Record<string, FakeHandler> => ({
  [`POST /v1/invites/${id}/resend`]: () => jsonResponse({ id, expires_at: EXPIRES, ...(url ? { invite_url: url } : {}) }),
});
const pendingAnswer = (email: string) => () => jsonResponse({ outcome: "existing_member", member: invited(email) }, 200);
const activeAnswer = (email: string) => () => jsonResponse({ outcome: "existing_member", member: active(email) }, 200);

describe("BP-1: what each address's answer means", () => {
  it("a_pending_invitee_goes_through_the_resend_path_and_says_invited_again", async () => {
    const { f } = setup({ "pend@x.test": pendingAnswer("pend@x.test") }, resend("inv-pend"));
    const dialog = await compose("pend@x.test, new@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    await screen.findByRole("dialog", { name: /^Invites sent/ });
    await waitFor(() => expect(post(f, "/v1/invites/inv-pend/resend")).toHaveLength(1));
    expect(await post(f, "/v1/invites/inv-pend/resend")[0].clone().json()).toEqual({ delivery: "email" });
    const item = screen.getByText("pend@x.test").closest("li")!;
    expect(item).toHaveTextContent(/Invited again with a new link/i);
    expect(item).not.toHaveTextContent(/already a member/i);
    // A re-invite is a real send: both count.
    expect(screen.getByRole("dialog", { name: "Invites sent: 2 of 2" })).toBeInTheDocument();
  });

  it("with_links_the_resent_link_is_listed_once_with_a_copy_button", async () => {
    setup({ "pend@x.test": pendingAnswer("pend@x.test") }, resend("inv-pend", "https://studio.test/invite#again-secret"), { email_available: false, reason: "email_not_configured" });
    const dialog = await compose("pend@x.test, new@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    const results = await screen.findByRole("dialog", { name: /^Invites sent/ });
    expect(within(results).getAllByDisplayValue("https://studio.test/invite#again-secret")).toHaveLength(1);
    expect(within(results).getByRole("button", { name: "Copy link for pend@x.test" })).toBeInTheDocument();
  });

  it("a_single_pending_invitee_is_resent_and_the_link_is_handed_over", async () => {
    const { onLink, f } = setup({ "pend@x.test": pendingAnswer("pend@x.test") }, resend("inv-pend", "https://studio.test/invite#again-secret"), { email_available: false, reason: "email_not_configured" });
    const dialog = await compose("pend@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Create link" }));
    await waitFor(() => expect(onLink).toHaveBeenCalledWith({ email: "pend@x.test", url: "https://studio.test/invite#again-secret" }));
    expect(post(f, "/v1/invites/inv-pend/resend")).toHaveLength(1);
  });

  it("a_single_pending_invitee_by_email_says_invited_again_not_already_a_member", async () => {
    const { onDone } = setup({ "pend@x.test": pendingAnswer("pend@x.test") }, resend("inv-pend"));
    const dialog = await compose("pend@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    await waitFor(() => expect(onDone).toHaveBeenCalledTimes(1));
    expect(onDone.mock.calls[0][0]).toMatch(/Invited pend@x\.test again/i);
    expect(onDone.mock.calls[0][0]).not.toMatch(/already a member/i);
  });

  it("an_active_member_is_told_so_sends_no_resend_and_is_not_counted_as_sent", async () => {
    const { f } = setup({ "ines@x.test": activeAnswer("ines@x.test") });
    const dialog = await compose("rui@x.test, ines@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    const results = await screen.findByRole("dialog", { name: "Invites sent: 1 of 2" });
    expect(screen.getByText("ines@x.test").closest("li")).toHaveTextContent(/already a member/i);
    expect(within(results).getByText("rui@x.test").closest("li")).not.toHaveTextContent(/already a member/i);
    expect(f.calls.filter((r) => new URL(r.url).pathname.endsWith("/resend"))).toHaveLength(0);
  });

  it("a_failure_carries_the_servers_reason_and_is_not_counted", async () => {
    setup({ "bad@x.test": () => jsonResponse({ error: { code: "forbidden", message: "only tenant admins may invite people" } }, 403) });
    const dialog = await compose("rui@x.test, bad@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    await screen.findByRole("dialog", { name: "Invites sent: 1 of 2" });
    const item = screen.getByText("bad@x.test").closest("li")!;
    expect(item).toHaveTextContent(/Not sent/i);
    expect(item).toHaveTextContent("only tenant admins may invite people");
    expect(item).not.toHaveTextContent(/already a member/i);
  });

  it("a_failed_resend_is_reported_as_not_sent_with_its_reason", async () => {
    setup({ "pend@x.test": pendingAnswer("pend@x.test") }, {
      "POST /v1/invites/inv-pend/resend": () => jsonResponse({ error: { code: "invite_closed", message: "the invite is no longer open" } }, 409),
    });
    const dialog = await compose("pend@x.test, new@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    await screen.findByRole("dialog", { name: "Invites sent: 1 of 2" });
    expect(screen.getByText("pend@x.test").closest("li")).toHaveTextContent(/Not sent/i);
  });

  it("edit_and_retry_reopens_with_exactly_the_failed_addresses", async () => {
    const fail = () => jsonResponse({ error: { code: "email_in_use_elsewhere", message: "x" } }, 409);
    setup({ "a@x.test": fail, "b@x.test": fail, "ines@x.test": activeAnswer("ines@x.test") });
    const dialog = await compose("a@x.test, ines@x.test, b@x.test, c@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 4 invites" }));
    await screen.findByRole("dialog", { name: /^Invites sent/ });
    await userEvent.click(screen.getByRole("button", { name: "Edit and retry the failed ones" }));
    const again = await screen.findByRole("dialog", { name: "Invite people" });
    const chips = within(again).queryAllByRole("button", { name: /^Remove \S+@/ }).map((b) => b.getAttribute("aria-label"));
    expect(chips).toEqual(["Remove a@x.test", "Remove b@x.test"]);
  });

  it("the_results_carry_a_subtitle_with_the_access_they_share", async () => {
    setup({});
    const dialog = await compose("rui@x.test, nadia@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    await userEvent.click(within(dialog).getByRole("radio", { name: "Viewer" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Send 2 invites" }));
    const results = await screen.findByRole("dialog", { name: "Invites sent: 2 of 2" });
    expect(results).toHaveTextContent(/Viewer on Shop/);
  });
});

describe("BP-2: Enter in the role search", () => {
  async function withRoles() {
    const s = setup({});
    const dialog = await compose("rui@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add a project" }));
    // Shop has no database; pick Blog, which has roles to search.
    await userEvent.selectOptions(within(dialog).getByLabelText("Project"), "Blog");
    const search = await within(dialog).findByRole("combobox", { name: "Postgres roles" });
    return { ...s, dialog, search };
  }

  it("enter_with_nothing_highlighted_does_not_submit", async () => {
    const { f, search } = await withRoles();
    await userEvent.click(search);
    await userEvent.keyboard("{Enter}");
    expect(post(f, "/v1/invites")).toHaveLength(0);
    await userEvent.type(search, "zzz{Enter}");
    expect(post(f, "/v1/invites")).toHaveLength(0);
    expect(screen.getByRole("dialog", { name: "Invite people" })).toBeInTheDocument();
  });

  it("the_send_button_still_sends", async () => {
    const { f, dialog } = await withRoles();
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    await waitFor(() => expect(post(f, "/v1/invites")).toHaveLength(1));
  });
});

describe("compose (fix 6)", () => {
  it("delivery_comes_before_access_and_after_the_emails", async () => {
    setup({});
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    const emails = within(dialog).getByLabelText("Emails");
    const delivery = await within(dialog).findByRole("radiogroup", { name: "How they get the invite" });
    const access = dialog.querySelector("legend")!; // Fix S5: "Access" and its note are spans inside the legend.
    const before = (a: Node, b: Node) => Boolean(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);
    expect(before(emails, delivery)).toBe(true);
    expect(before(delivery, access)).toBe(true);
  });

  it("access_rows_use_the_tab_tone_and_a_themed_project_select_and_add_a_project_has_a_plus", async () => {
    setup({});
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    const add = within(dialog).getByRole("button", { name: "Add a project" });
    expect(add.querySelector("svg")).not.toBeNull();
    await userEvent.click(add);
    const row = within(dialog).getByRole("group", { name: "Shop" });
    for (const radio of within(row).getAllByRole("radio")) expect(radio.className).toContain("bg-channel");
    expectThemedSelects(row);
  });

  it("email_chips_are_mono", async () => {
    setup({});
    const dialog = await compose("rui@x.test");
    expect(within(dialog).getByText("rui@x.test").closest(".font-mono")).not.toBeNull();
  });
});
