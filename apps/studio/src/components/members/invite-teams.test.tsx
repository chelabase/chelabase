import { useState } from "react";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Member, Project, ProjectGrant, Team } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { ApproveDialog } from "./approve-dialog";
import { InviteDialog } from "./invite-dialog";
import { rowsFromMember } from "./copy-from-member";
import { toggleTeam } from "./team-picker";
import { person } from "./hub/fixtures";

const projects = [
  { id: "p1", slug: "shop", name: "Shop", status: "active", databases: [] },
  { id: "p2", slug: "blog", name: "Blog", status: "active", databases: [] },
] as unknown as Project[];
const team = (id: string, name: string) => ({ id, name, description: "", member_count: 2, projects: [], created_at: "", updated_at: "" }) as unknown as Team;
const TEAMS = [team("t-support", "Support"), team("t-billing", "Billing")];
const pending: Member = { ...person("pend", { status: "pending", name: "Pat Pending" }), email: "pat@x.test" };
const sofia = person("sofia", { name: "Sofia", level: "developer", projects: [["p1", "developer"]] });

const grant = (member_id: string, platform_role: string, pg_roles: string[] | "all"): ProjectGrant => ({ member_id, platform_role, pg_roles, overrides: [] }) as unknown as ProjectGrant;

function base(extra: Record<string, FakeHandler> = {}) {
  return fakeFetch({
    "GET /v1/invites/delivery": () => jsonResponse({ email_available: true, reason: null }),
    "GET /v1/teams": () => jsonResponse(TEAMS),
    "GET /v1/members": () => jsonResponse([sofia]),
    "GET /v1/projects/p1/grants": () => jsonResponse([grant("sofia", "developer", ["shop_reader"])]),
    "GET /v1/projects/p2/grants": () => jsonResponse([]),
    ...extra,
  });
}
const posts = (f: { calls: Request[] }, path: string) => f.calls.filter((r) => r.method === "POST" && new URL(r.url).pathname === path);

function InviteHarness() {
  const [open, setOpen] = useState(true);
  return <InviteDialog open={open} onOpenChange={setOpen} projects={projects} onDone={() => setOpen(false)} onLink={() => {}} />;
}

describe("teams in the tenant invite", () => {
  it("invite_sends_team_ids_on_the_tenant_invite_only", async () => {
    const f = base({ "POST /v1/invites": () => jsonResponse({ outcome: "invited", member: person("x", { status: "invited" }) }, 201) });
    renderWithQuery(<InviteHarness />, { fetch: f });
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.click(within(dialog).getByLabelText("Emails"));
    await userEvent.paste("rui@x.test");
    await userEvent.click(await within(dialog).findByRole("checkbox", { name: "Team Support" }));
    await userEvent.click(within(dialog).getByRole("checkbox", { name: "Team Billing" }));
    await userEvent.click(within(dialog).getByRole("checkbox", { name: "Team Support" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    await waitFor(() => expect(posts(f, "/v1/invites")).toHaveLength(1));
    expect((await posts(f, "/v1/invites")[0].clone().json()).team_ids).toEqual(["t-billing"]);
  });

  it("no_team_chosen_sends_no_team_ids", async () => {
    const f = base({ "POST /v1/invites": () => jsonResponse({ outcome: "invited", member: person("x", { status: "invited" }) }, 201) });
    renderWithQuery(<InviteHarness />, { fetch: f });
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await within(dialog).findByRole("checkbox", { name: "Team Support" });
    await userEvent.click(within(dialog).getByLabelText("Emails"));
    await userEvent.paste("rui@x.test");
    await userEvent.click(within(dialog).getByRole("button", { name: "Send invite" }));
    await waitFor(() => expect(posts(f, "/v1/invites")).toHaveLength(1));
    expect(await posts(f, "/v1/invites")[0].clone().json()).not.toHaveProperty("team_ids");
  });

  it("copy_from_a_member_fills_own_grants_not_teams", async () => {
    const f = base();
    renderWithQuery(<InviteHarness />, { fetch: f });
    const dialog = await screen.findByRole("dialog", { name: "Invite people" });
    await userEvent.selectOptions(await within(dialog).findByRole("combobox", { name: "Copy from a member" }), "sofia");
    await userEvent.click(within(dialog).getByRole("button", { name: "Copy" }));
    expect(await within(dialog).findByRole("group", { name: "Shop" })).toBeInTheDocument();
    expect(within(dialog).queryByRole("group", { name: "Blog" })).toBeNull();
    // Only the grants lists were read; the member's teams never.
    expect(f.calls.some((r) => new URL(r.url).pathname.endsWith("/teams") && new URL(r.url).pathname.startsWith("/v1/members/"))).toBe(false);
    // The team checklist stays as it was.
    expect(within(dialog).getByRole("checkbox", { name: "Team Support" })).not.toBeChecked();
  });
});

describe("rowsFromMember", () => {
  const fetchGrants = async (id: string) => (id === "p1" ? [grant("sofia", "developer", ["a", "b"]), grant("other", "viewer", [])] : id === "p2" ? [grant("sofia", "admin", "all")] : []);
  it("copies_own_grants_only_and_names_no_roles_for_admin", async () => {
    const rows = await rowsFromMember("sofia", projects, fetchGrants);
    expect(rows).toEqual([
      { projectId: "p1", platformRole: "developer", pgRoles: ["a", "b"], blocked: false },
      { projectId: "p2", platformRole: "admin", pgRoles: [], blocked: false },
    ]);
  });
  it("skips_projects_being_deleted_and_unreadable_lists", async () => {
    const ps = [{ ...projects[0], status: "deleting" }, projects[1]] as unknown as Project[];
    expect(await rowsFromMember("sofia", ps, fetchGrants)).toHaveLength(1);
    expect(await rowsFromMember("sofia", projects, async () => { throw new Error("403"); })).toEqual([]);
  });
});

describe("toggleTeam", () => {
  it("adds_once_and_removes_in_place", () => {
    expect(toggleTeam(["a"], "b")).toEqual(["a", "b"]);
    expect(toggleTeam(["a", "b", "c"], "b")).toEqual(["a", "c"]);
  });
});

describe("teams in the approval", () => {
  it("approve_sends_team_ids", async () => {
    const f = base({ "POST /v1/members/pend/approve": () => jsonResponse({ ...pending, status: "active" }) });
    renderWithQuery(<ApproveDialog member={pending} onClose={() => {}} projects={projects} onDone={() => {}} />, { fetch: f });
    const dialog = await screen.findByRole("dialog", { name: /Approve/ });
    await userEvent.click(await within(dialog).findByRole("checkbox", { name: "Team Billing" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Approve" }));
    await waitFor(() => expect(posts(f, "/v1/members/pend/approve")).toHaveLength(1));
    const body = await posts(f, "/v1/members/pend/approve")[0].clone().json();
    expect(body.team_ids).toEqual(["t-billing"]);
    expect(body.grants).toEqual([]);
  });
});
