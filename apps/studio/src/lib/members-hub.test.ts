import { describe, expect, it } from "vitest";
import type { AccessEvent, Member } from "@chelabase/management-client";
import { eventFields, eventSentence, memberStatus } from "./members";

const NOW = new Date("2026-10-23T12:00:00Z");

function suspended(): Member {
  return {
    id: "m1", email: "ana@example.test", display_name: "Ana", status: "suspended", tenant_admin: false, has_access: true,
    invite: null, accepted_at: null, suspended_at: null, suspended_by: null, created_at: "2026-10-01T00:00:00Z",
    access: { level: "developer", projects: [] },
  };
}

function event(action: string, extra: Partial<AccessEvent> = {}): AccessEvent {
  return {
    id: 1, action, at: "2026-10-23T11:00:00Z", actor_member_id: "m1", actor_email: "ana@example.test",
    target_member_id: "m2", target_email: "bo@example.test", project_id: null, project_name: null, database_id: null,
    branch_id: null, before: null, after: null, ...extra,
  };
}

const ctx = { names: new Map([["m1", "Ana"], ["m2", "Bo"]]), databaseName: () => "orders" };

describe("lib/members", () => {
  it("new_actions_and_the_suspended_status_have_words", () => {
    expect(memberStatus(suspended(), NOW)).toMatchObject({ text: "Suspended", tone: "hazard" });
    expect(eventFields({ status: "suspended" }, ctx)).toEqual([{ label: "Status", value: "Suspended" }]);

    const actions = [
      "member_suspended", "member_reinstated", "personal_logins_revoked", "personal_login_created", "personal_login_reset",
      "personal_login_deleted", "role_created", "role_updated", "role_deleted", "privileges_changed",
    ];
    const sentences = actions.map((a) => eventSentence(event(a), ctx));
    for (const [i, s] of sentences.entries()) {
      // Never the fallback that prints the raw action name.
      expect(s, actions[i]).not.toContain(actions[i]);
      expect(s, actions[i]).not.toMatch(/changed access \(/);
      expect(s, actions[i]).toContain("Ana");
    }
    // Each says something different.
    expect(new Set(sentences).size).toBe(actions.length);
    expect(eventSentence(event("member_suspended"), ctx)).toContain("Bo");
    expect(eventSentence(event("member_reinstated"), ctx)).toContain("Bo");
  });
});
