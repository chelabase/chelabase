import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import type { AccessEvent, MeResponse } from "@chelabase/management-client";
import { person } from "../hub/fixtures";
import { defaultLoginName, withDefaultRole } from "./logins";
import {
  CONNECTION, D2, LOGIN_LATER, LOGIN_REVOKED, LOGIN_SOON, MARA, P1, P2, PASSWORD, PRI, SOFIA, calls, daysAgo, login, meFor, mount, ok,
  secret, sofiaFor, type Opts,
} from "./test-setup";

/** The coder's own tests (task S2): edges beyond the RED set. */

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const ALL = () => [LOGIN_SOON, LOGIN_LATER, LOGIN_REVOKED];
const RESET_D2 = `POST /v1/databases/${D2}/personal-login/reset-password`;
const self = (opts: Opts = {}) => mount({ caller: "self", memberId: SOFIA, members: () => [sofiaFor("self")], logins: ALL, ...opts });
const rowOf = async (name: RegExp) => within(await screen.findByRole("table", { name: "Personal Postgres logins" })).getByRole("row", { name });

describe("the default role in a connection string (P33): pure edges", () => {
  it("adds nothing for none, ? or & as the string needs, and escapes what libpq would split", () => {
    expect(withDefaultRole(CONNECTION, "")).toBe(CONNECTION);
    expect(withDefaultRole("postgresql://a@h/db", "g")).toBe("postgresql://a@h/db?options=-c%20role%3Dg");
    expect(withDefaultRole("postgresql://a@h/db?sslmode=require", "g")).toBe("postgresql://a@h/db?sslmode=require&options=-c%20role%3Dg");
    // A space would split the option in two; a backslash escapes: both are backslash-escaped, then URL-encoded.
    expect(withDefaultRole("postgresql://a@h/db", "my group")).toBe("postgresql://a@h/db?options=-c%20role%3Dmy%5C%20group");
    expect(withDefaultRole("postgresql://a@h/db", "a\\b")).toBe("postgresql://a@h/db?options=-c%20role%3Da%5C%5Cb");
    expect(withDefaultRole("postgresql://a@h/db", "x&options=-c%20role%3Dpostgres")).not.toContain("&options=-c%20role%3Dpostgres");
  });

  it("the default login name is always a valid login name", () => {
    for (const email of [null, "", "@x.test", "ÄÖÜ@x.test", "...@x.test", "a b@x.test", "UPPER.case@x.test", "9@x.test", "x".repeat(200)]) {
      expect(defaultLoginName(email)).toMatch(/^[a-z_][a-z0-9_]{0,62}$/);
    }
  });
});

describe("the default role on a group name with a special character", () => {
  it("url_encodes_an_ampersand_in_the_dialog", async () => {
    const me = meFor("self") as unknown as { projects: { project_id: string; roles: unknown; databases: { roles: unknown }[] }[] };
    const roles = ["etl_writer", "a&b"];
    const patched = { ...me, projects: me.projects.map((p) => (p.project_id === P2 ? { ...p, roles, databases: p.databases.map((d) => ({ ...d, roles })) } : p)) };
    const m = self({ routes: { "GET /v1/me": ok(patched), [RESET_D2]: ok(secret(LOGIN_LATER, { connection_string: CONNECTION })) } });
    await m.user.click(within(await rowOf(/sofia_m/)).getByRole("button", { name: "Reset password" }));
    const dialog = await screen.findByRole("dialog", { name: "Your login sofia_m on warehouse" });
    await m.user.selectOptions(within(dialog).getByLabelText("Default role"), "a&b");
    expect(within(dialog).getByLabelText("Connection string · main branch")).toHaveValue(`${CONNECTION}&options=-c%20role%3Da%26b`);
  });
});

describe("a login row's own connection string", () => {
  // `has_no_password_and_takes_a_default_role` moved into the row's menu (P36): see member-page-fs.test.tsx.

  it("is_not_offered_for_a_revoked_or_expired_login_or_on_someone_elses_page", async () => {
    const expired = login({ role_name: "sofia_old", database_id: D2, status: "expired", valid_until: daysAgo(3) });
    self({ logins: () => [LOGIN_REVOKED, expired] });
    const table = await screen.findByRole("table", { name: "Personal Postgres logins" });
    expect(within(table).queryByRole("button", { name: /Copy connection string/ })).toBeNull();
    expect(within(table).queryByLabelText(/Default role/)).toBeNull();
  });

  it("an_admin_does_not_get_one_for_a_login_that_is_not_theirs", async () => {
    mount({ caller: "tenant_admin", logins: ALL });
    const table = await screen.findByRole("table", { name: "Personal Postgres logins" });
    expect(within(table).queryByRole("button", { name: /Copy connection string/ })).toBeNull();
  });
});

describe("the reveal dialog", () => {
  it("a_click_outside_does_not_close_it_and_leaving_the_page_drops_the_password", async () => {
    const m = self({ routes: { [RESET_D2]: ok(secret(LOGIN_LATER)) } });
    await m.user.click(within(await rowOf(/sofia_m/)).getByRole("button", { name: "Reset password" }));
    await screen.findByRole("dialog", { name: "Your login sofia_m on warehouse" });
    await m.user.click(document.body);
    expect(screen.getByRole("dialog", { name: "Your login sofia_m on warehouse" })).toBeInTheDocument();
    await m.router.navigate({ href: "/other" });
    expect(await screen.findByText("elsewhere")).toBeInTheDocument();
    expect(document.body.innerHTML).not.toContain(PASSWORD);
  });
});

describe("Take access away", () => {
  it("asks_then_deletes_the_grant_of_that_project_only", async () => {
    const m = mount({ caller: "tenant_admin", routes: { [`DELETE /v1/projects/${P1}/grants/${SOFIA}`]: () => new Response(null, { status: 204 }) } });
    const billing = await screen.findByRole("region", { name: "Billing app" });
    await m.user.click(await within(billing).findByRole("button", { name: "Take access away" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Take Sofia Marchetti's access to Billing app away?" });
    expect(calls(m.f, "DELETE", "/v1/projects")).toHaveLength(0);
    await m.user.click(within(dialog).getByRole("button", { name: "Take access away" }));
    await waitFor(() => expect(calls(m.f, "DELETE", "/v1/projects")).toHaveLength(1));
    expect(new URL(calls(m.f, "DELETE", "/v1/projects")[0].url).pathname).toBe(`/v1/projects/${P1}/grants/${SOFIA}`);
  });
});

describe("no pronoun is guessed from a name", () => {
  it("the_suspend_and_revoke_copy_has_none", async () => {
    const m = mount({ caller: "tenant_admin", logins: () => [LOGIN_LATER] });
    await m.user.click(await screen.findByRole("button", { name: "Suspend" }));
    const suspend = await screen.findByRole("alertdialog", { name: "Suspend Sofia Marchetti?" });
    expect(suspend.textContent).not.toMatch(/\b(she|her|hers|he|him|his)\b/i);
    await m.user.click(within(suspend).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    await m.user.click(screen.getByRole("button", { name: "Revoke Postgres access now" }));
    const revoke = await screen.findByRole("alertdialog", { name: "Revoke Sofia's Postgres access now?" });
    expect(revoke.textContent).not.toMatch(/\b(she|her|hers|he|him|his)\b/i);
  });
});

describe("Activity for a project Admin of several projects", () => {
  const event = (id: number, action: string, project: string): AccessEvent => ({
    id, action, at: "2026-10-20T10:00:00Z", actor_member_id: MARA, actor_email: "mara@northwind.dev", target_member_id: SOFIA, target_email: "sofia@northwind.dev",
    project_id: project, project_name: null, database_id: null, branch_id: null, before: null, after: null,
  } as AccessEvent);

  it("merges_each_administered_projects_log_newest_first_without_the_tenant_log", async () => {
    const base = meFor("tenant_admin") as unknown as { member: unknown };
    const asProjectAdmin = { ...base, member: { id: PRI, status: "active", tenant_admin: false } } as unknown as MeResponse;
    const m = mount({
      caller: "tenant_admin",
      routes: {
        "GET /v1/me": ok(asProjectAdmin),
        [`GET /v1/projects/${P1}/access-events`]: ok({ events: [event(9, "admin_set", P1), event(3, "admin_unset", P1)], next_before: null }),
        [`GET /v1/projects/${P2}/access-events`]: ok({ events: [event(7, "admin_unset", P2)], next_before: null }),
      },
      members: () => [sofiaFor("tenant_admin"), person(MARA, { name: "Mara Okafor", level: "tenant_admin" }), person(PRI, { name: "Priya Raman", level: "admin", projects: [[P1, "admin"], [P2, "admin"]] })],
    });
    await screen.findByText("Mara Okafor made Sofia Marchetti a tenant admin");
    const list = within(screen.getByRole("region", { name: "Activity" })).getAllByRole("listitem");
    expect(list.map((li) => /made|removed/.exec(li.textContent ?? "")?.[0])).toEqual(["made", "removed", "removed"]);
    expect(calls(m.f, "GET", "/v1/access-events")).toHaveLength(0);
  });
});
