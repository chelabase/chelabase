import { describe, expect, it } from "vitest";
import type { AccessEvent, Member } from "@chelabase/management-client";
import { eventFields, eventSentence, memberName, memberStatus, relativeTime } from "./members";

const NOW = new Date("2026-10-09T12:00:00Z");

function member(extra: Partial<Member> = {}): Member {
  return {
    id: "m1", email: "ana@example.test", display_name: "Ana", status: "active", tenant_admin: false, has_access: true,
    invite: null, accepted_at: null, created_at: "2026-10-01T00:00:00Z", ...extra,
  };
}

function event(extra: Partial<AccessEvent>): AccessEvent {
  return {
    id: 1, action: "member_removed", at: "2026-10-09T11:00:00Z", actor_member_id: "m1", actor_email: "ana@example.test",
    target_member_id: "m2", target_email: "bo@example.test", project_id: null, project_name: null, database_id: null,
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
