import { describe, expect, it } from "vitest";
import type { AccessEvent, Member } from "@chelabase/management-client";
import { eventFields, eventSentence, memberName, memberStatus, relativeTime } from "./members";

const NOW = new Date("2026-10-09T12:00:00Z");

function member(extra: Partial<Member> = {}): Member {
  return {
    id: "m1", email: "ana@example.test", display_name: "Ana", status: "active", tenant_admin: false, has_access: true,
    invite: null, accepted_at: null, suspended_at: null, suspended_by: null, created_at: "2026-10-01T00:00:00Z", access: { level: "none", projects: [] }, ...extra,
  };
}

function event(extra: Partial<AccessEvent>): AccessEvent {
  return {
    id: 1, action: "member_removed", at: "2026-10-09T11:00:00Z", actor_member_id: "m1", actor_email: "ana@example.test",
    target_member_id: "m2", target_email: "bo@example.test", project_id: null, project_name: null, database_id: null, branch_id: null,
    before: null, after: null, ...extra,
  };
}

describe("memberName", () => {
  it("falls back to the email's local part, then to a placeholder", () => {
    expect(memberName(member())).toBe("Ana");
    expect(memberName(member({ display_name: "" }))).toBe("ana");
    expect(memberName(member({ display_name: "", email: null }))).toBe("Unnamed member");
  });
});

describe("memberStatus", () => {
  it("writes the status as words", () => {
    expect(memberStatus(member(), NOW)).toEqual({ text: "Active", tone: "plain" });
    expect(memberStatus(member({ has_access: false }), NOW)).toEqual({ text: "No access yet", tone: "dim" });
    expect(memberStatus(member({ has_access: false, tenant_admin: true }), NOW)).toEqual({ text: "Active", tone: "plain" });
    expect(
      memberStatus(member({ status: "invited", invite: { id: "i1", expires_at: "2026-10-11T12:00:00Z" } }), NOW),
    ).toEqual({ text: "Invited · expires in 2 days", tone: "plain", title: "2026-10-11 12:00 UTC" });
    expect(
      memberStatus(member({ status: "invited", invite: { id: "i1", expires_at: "2026-10-09T11:00:00Z" } }), NOW),
    ).toEqual({ text: "Invite expired", tone: "hazard", title: "2026-10-09 11:00 UTC" });
    expect(memberStatus(member({ status: "pending" }), NOW)).toEqual({ text: "Waiting for approval", tone: "dim" });
  });
});

describe("relativeTime", () => {
  it("says past and future times in the largest whole unit", () => {
    expect(relativeTime("2026-10-09T11:59:40Z", NOW)).toBe("just now");
    expect(relativeTime("2026-10-09T11:00:00Z", NOW)).toBe("1 hour ago");
    expect(relativeTime("2026-10-06T12:00:00Z", NOW)).toBe("3 days ago");
    expect(relativeTime("2026-10-09T12:05:00Z", NOW)).toBe("in 5 minutes");
  });
});

describe("eventSentence", () => {
  const names = new Map([["m1", "Ana"]]);
  const ctx = { names, databaseName: (id: string) => (id === "d1" ? "orders" : null) };

  it("names people from the members list, else the event's email, else a former member", () => {
    expect(eventSentence(event({}), ctx)).toBe("Ana removed bo@example.test");
    expect(eventSentence(event({ target_email: null }), ctx)).toBe("Ana removed a former member");
    expect(eventSentence(event({ actor_member_id: null, action: "admin_claimed" }), ctx)).toBe(
      "bo@example.test became the first tenant admin",
    );
  });

  it("covers grants, overrides, invites, sign-up switches, renames and approvals", () => {
    expect(
      eventSentence(event({ action: "grant_put", project_name: "Shop", after: { platform_role: "developer", pg_roles: [] } }), ctx),
    ).toBe("Ana gave bo@example.test Developer on Shop");
    expect(
      eventSentence(
        event({
          action: "grant_put", project_name: "Shop",
          before: { platform_role: "viewer", pg_roles: [] }, after: { platform_role: "admin", pg_roles: [] },
        }),
        ctx,
      ),
    ).toBe("Ana changed bo@example.test's access on Shop: Viewer → Admin");
    // Only the Postgres roles changed: the sentence names them, never "Developer → Developer".
    expect(
      eventSentence(
        event({
          action: "grant_put", project_name: "Shop",
          before: { platform_role: "developer", pg_roles: [] }, after: { platform_role: "developer", pg_roles: ["ops", "reporting"] },
        }),
        ctx,
      ),
    ).toBe("Ana changed bo@example.test's Postgres roles on Shop: none → ops, reporting");
    expect(eventSentence(event({ action: "grant_deleted", project_name: "Shop" }), ctx)).toBe(
      "Ana took bo@example.test's access to Shop away",
    );
    expect(
      eventSentence(
        event({ action: "override_put", database_id: "d1", after: { platform_role: "viewer", pg_roles: ["billing"] } }),
        ctx,
      ),
    ).toBe("Ana narrowed bo@example.test on orders to Viewer, billing");
    expect(eventSentence(event({ action: "override_deleted", database_id: "dx" }), ctx)).toBe(
      "Ana stopped narrowing bo@example.test on a deleted database",
    );
    expect(eventSentence(event({ action: "invite_created", after: { email: "new@example.test" } }), ctx)).toBe(
      "Ana invited new@example.test",
    );
    expect(eventSentence(event({ action: "invite_accepted" }), ctx)).toBe("bo@example.test accepted the invite");
    expect(eventSentence(event({ action: "open_signup_set", after: { open_signup: true } }), ctx)).toBe(
      "Ana turned open sign-up on",
    );
    expect(eventSentence(event({ action: "tenant_renamed", after: { display_name: "Acme" } }), ctx)).toBe(
      "Ana renamed the tenant to Acme",
    );
    expect(eventSentence(event({ action: "tenant_renamed", after: { display_name: null } }), ctx)).toBe(
      "Ana cleared the tenant's name",
    );
    expect(eventSentence(event({ action: "member_approved" }), ctx)).toBe("Ana approved bo@example.test's sign-up with no project access");
    // The approval says what access it gave (project names from the context).
    const withProjects = { ...ctx, projectName: (id: string) => ({ p1: "Shop", p2: "Blog" })[id] ?? null };
    expect(
      eventSentence(
        event({
          action: "member_approved",
          after: { status: "active", grants: [{ project_id: "p1", platform_role: "developer", pg_roles: ["billing"] }, { project_id: "p2", platform_role: "viewer", pg_roles: [] }] },
        }),
        withProjects,
      ),
    ).toBe("Ana approved bo@example.test's sign-up with Developer on Shop and Viewer on Blog");
    expect(
      eventSentence(event({ action: "member_approved", after: { grants: [{ project_id: "p9", platform_role: "admin", pg_roles: [] }] } }), withProjects),
    ).toBe("Ana approved bo@example.test's sign-up with Admin on a deleted project");
    // A sign-up joins as pending: it "signed up"; an org user admitted at once "signed in for the first time".
    expect(eventSentence(event({ action: "member_joined", after: { status: "pending", tenant_admin: false } }), ctx)).toBe("bo@example.test signed up");
    expect(eventSentence(event({ action: "member_joined", after: { status: "active", tenant_admin: false } }), ctx)).toBe(
      "bo@example.test signed in for the first time",
    );
    expect(eventSentence(event({ action: "member_declined" }), ctx)).toBe("Ana declined bo@example.test's sign-up");
    expect(eventSentence(event({ action: "something_new", actor_member_id: null, actor_email: null }), ctx)).toBe(
      "Chelabase changed access (something_new)",
    );
  });
});

describe("eventFields", () => {
  const ctx = {
    names: new Map<string, string>(),
    databaseName: (id: string) => (id === "d1" ? "orders" : null),
    projectName: (id: string) => (id === "p1" ? "Shop" : null),
  };

  // The browser pass: details were raw JSON with project ids; they are labelled fields now.
  it("labels known fields, names projects and databases, and keeps unknown ones readable", () => {
    expect(
      eventFields(
        {
          status: "active",
          grants: [{ project_id: "p1", platform_role: "developer", pg_roles: ["analyst", "billing"] }, { project_id: "gone", platform_role: "viewer", pg_roles: [] }],
          expires_at: "2026-10-12T16:05:39.171279394Z",
          email: "bo@example.test",
          tenant_admin: true,
          open_signup: false,
          display_name: null,
          database_id: "d1",
          platform_role: "viewer",
          pg_roles: null,
          surprise: { a: 1 },
        },
        ctx,
      ),
    ).toEqual([
      { label: "Status", value: "Active" },
      { label: "Access", value: "Developer on Shop (analyst, billing); Viewer on a deleted project" },
      { label: "Expires", value: "2026-10-12 16:05 UTC" },
      { label: "Email", value: "bo@example.test" },
      { label: "Tenant admin", value: "Yes" },
      { label: "Open sign-up", value: "Off" },
      { label: "Tenant name", value: "None (shows the slug)" },
      { label: "Database", value: "orders" },
      { label: "Platform role", value: "Viewer" },
      { label: "Postgres roles", value: "All of the project's roles" },
      { label: "surprise", value: '{"a":1}' },
    ]);
    expect(eventFields(null, ctx)).toEqual([]);
    expect(eventFields({ pg_roles: [] }, ctx)).toEqual([{ label: "Postgres roles", value: "None" }]);
  });
});

describe("team events", () => {
  const ctx = { names: new Map([["m1", "Ana"], ["m2", "Bo"]]), databaseName: () => null, projectName: (id: string) => (id === "p1" ? "Shop" : null) };
  const team = { id: "t1", name: "Billing" };
  const grant = (platform_role: string, pg_roles: string[] = []) => ({ project_id: "p1", platform_role, pg_roles });
  const say = (extra: Partial<AccessEvent>) => eventSentence(event(extra), ctx);

  it("team_events_have_words", () => {
    expect(say({ action: "team_created", target_member_id: null, target_email: null, after: { team, grants: [], affected_member_ids: [] } })).toBe("Ana created the team Billing");
    expect(say({ action: "team_updated", target_member_id: null, target_email: null, before: { team }, after: { team: { id: "t1", name: "Finance" } } })).toBe(
      "Ana renamed the team Billing to Finance",
    );
    expect(say({ action: "team_updated", target_member_id: null, target_email: null, before: { team }, after: { team } })).toBe("Ana changed the team Billing");
    expect(say({ action: "team_deleted", target_member_id: null, target_email: null, before: { team, grants: [], affected_member_ids: ["m2"] } })).toBe("Ana deleted the team Billing");
    expect(say({ action: "team_grant_put", target_member_id: null, target_email: null, after: { team, grants: [grant("developer")] } })).toBe("Ana gave the team Billing Developer on Shop");
    expect(
      say({ action: "team_grant_put", target_member_id: null, target_email: null, before: { team, grants: [grant("viewer")] }, after: { team, grants: [grant("admin")] } }),
    ).toBe("Ana changed the team Billing's access on Shop: Viewer → Admin");
    expect(
      say({ action: "team_grant_put", target_member_id: null, target_email: null, before: { team, grants: [grant("developer", ["a"])] }, after: { team, grants: [grant("developer", ["b"])] } }),
    ).toBe("Ana changed the team Billing's Postgres roles on Shop");
    expect(say({ action: "team_grant_deleted", target_member_id: null, target_email: null, before: { team, grants: [grant("viewer")] } })).toBe("Ana took the team Billing's access to Shop away");
    expect(say({ action: "team_member_added", after: { team, affected_member_ids: ["m2"] } })).toBe("Ana added Bo to the team Billing");
    expect(say({ action: "team_member_removed", target_member_id: "m2", before: { team } })).toBe("Ana removed Bo from the team Billing");
  });

  it("team_names_are_escaped_in_sentences_and_fields", () => {
    const odd = { id: "t1", name: "Bi\u202Eg\\" };
    const shown = "Bi\\u202eg\\\\";
    expect(say({ action: "team_created", target_member_id: null, target_email: null, after: { team: odd, grants: [], affected_member_ids: [] } })).toBe(`Ana created the team ${shown}`);
    expect(say({ action: "team_updated", target_member_id: null, target_email: null, before: { team: odd }, after: { team } })).toBe(`Ana renamed the team ${shown} to Billing`);
    expect(say({ action: "team_member_added", after: { team: odd, affected_member_ids: ["m2"] } })).toBe(`Ana added Bo to the team ${shown}`);
    expect(eventFields({ team: odd }, ctx)).toEqual([{ label: "Team", value: shown }]);
  });

  it("falls_back_when_the_team_or_project_is_gone", () => {
    expect(say({ action: "team_deleted", target_member_id: null, target_email: null })).toBe("Ana deleted the team a deleted team");
    expect(say({ action: "team_grant_deleted", target_member_id: null, target_email: null, before: { team, grants: [{ ...grant("viewer"), project_id: "gone" }] } })).toBe(
      "Ana took the team Billing's access to a deleted project away",
    );
  });

  it("shows_the_team_and_who_it_reaches_as_fields_and_nothing_for_a_projects_empty_list", () => {
    expect(eventFields({ team, grants: [grant("viewer")], affected_member_ids: ["m1", "m2", "zz"] }, ctx, "team_grant_put")).toEqual([
      { label: "Team", value: "Billing" },
      { label: "Access", value: "Viewer on Shop" },
      { label: "People reached", value: "Ana, Bo, a former member" },
    ]);
    expect(eventFields({ team, affected_member_ids: [] }, ctx).map((f) => f.label)).toEqual(["Team"]);
  });
});

