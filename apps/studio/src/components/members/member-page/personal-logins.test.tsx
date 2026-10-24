import { screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it } from "vitest";
import type { MeResponse, PersonalLoginView } from "@chelabase/management-client";
import { jsonResponse } from "@/test/fake-fetch";
import {
  CONNECTION, D1, D2, LOGIN_LATER, LOGIN_REVOKED, LOGIN_SOON, P2, PASSWORD, SOFIA, apiError, calls, daysAgo, inDays, login, meFor, mount, ok,
  secret, sofiaFor, type Opts,
} from "./test-setup";

beforeEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

const ALL = () => [LOGIN_SOON, LOGIN_LATER, LOGIN_REVOKED];
const RESET_D2 = `POST /v1/databases/${D2}/personal-login/reset-password`;
const RESET_D1 = `POST /v1/databases/${D1}/personal-login/reset-password`;

/** Sofia on her own page. */
function self(opts: Opts = {}) {
  return mount({ caller: "self", memberId: SOFIA, members: () => [sofiaFor("self")], logins: ALL, ...opts });
}
const rowOf = async (name: RegExp) => within(await screen.findByRole("table", { name: "Personal Postgres logins" })).getByRole("row", { name });

/** Opens the reveal dialog by resetting sofia_m's password. */
async function reveal(over: Opts = {}, answer: ReturnType<typeof secret> = secret(LOGIN_LATER, { connection_string: CONNECTION })) {
  const m = self({ ...over, routes: { [RESET_D2]: ok(answer), ...over.routes } });
  await m.user.click(within(await rowOf(/sofia_m/)).getByRole("button", { name: "Reset password" }));
  const dialog = await screen.findByRole("dialog", { name: "Your login sofia_m on warehouse" });
  return { ...m, dialog };
}

/** Everything the page could have kept the password in, as one string. */
function leaks(m: ReturnType<typeof self>) {
  const stores: string[] = [];
  for (const s of [localStorage, sessionStorage]) for (let i = 0; i < s.length; i++) stores.push(`${s.key(i)}=${s.getItem(s.key(i)!)}`);
  return JSON.stringify([
    m.queryClient.getQueryCache().getAll().map((q) => [q.queryKey, q.state.data]),
    m.queryClient.getMutationCache().getAll().map((x) => [x.state.variables, x.state.data, x.state.error]),
    stores,
    m.router.state.location.href,
    window.location.href,
    JSON.stringify(window.history.state),
  ]);
}

describe("Personal Postgres logins: the list", () => {
  it("lists_database_login_expires_created_and_what_is_revoked", async () => {
    self();
    const table = await screen.findByRole("table", { name: "Personal Postgres logins" });
    for (const h of ["Database", "Login", "Expires", "Created"]) expect(within(table).getByRole("columnheader", { name: h })).toBeInTheDocument();
    const later = within(table).getByRole("row", { name: /sofia_m/ });
    expect(within(later).getByText("warehouse")).toBeInTheDocument();
    expect(within(later).getByText(/in 71 days/)).toBeInTheDocument();
    const revoked = within(table).getByRole("row", { name: /Revoked/ });
    expect(within(revoked).getByRole("button", { name: "Delete" })).toBeInTheDocument();
    expect(within(revoked).queryByRole("button", { name: /Reset password|Renew/ })).toBeNull();
  });

  it("says_where_a_login_works_for_the_member_themself", async () => {
    self();
    expect(await screen.findByText(/A login works on every branch of its database where you have Developer access or more/)).toBeInTheDocument();
    expect(screen.getByText(/Connection strings are for the main branch; on another branch, use that branch's host/)).toBeInTheDocument();
  });

  it("an_admin_sees_delete_only_and_who_can_renew", async () => {
    mount({ caller: "tenant_admin", logins: ALL });
    const later = await rowOf(/sofia_m/);
    expect(within(later).getByRole("button", { name: "Delete" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Reset password|Renew|New login/ })).toBeNull();
    expect(screen.getByText(/Only Sofia can create, reset or renew (her|their|his) logins/)).toBeInTheDocument();
    expect(screen.getByText(/where Sofia has Developer access or more/)).toBeInTheDocument();
  });

  it("an_empty_list_says_so_and_a_viewer_cannot_make_one", async () => {
    const viewerMe = meFor("self") as unknown as { projects: { platform_role: string }[]; tenant: unknown };
    const asViewer = { ...viewerMe, projects: viewerMe.projects.map((p) => ({ ...p, platform_role: "viewer" })) } as unknown as MeResponse;
    self({ logins: () => [], routes: { "GET /v1/me": ok(asViewer) } });
    expect(await screen.findByText(/No personal logins yet/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New login" })).toBeNull();
  });
});

describe("Personal Postgres logins: New login", () => {
  it.each([
    ["Sofia.Marchetti+Work@northwind.dev", "sofia_marchetti_work"],
    ["9lives@x.test", "_9lives"],
    ["a".repeat(80) + "@x.test", "a".repeat(63)],
    ["SOFIA@northwind.dev", "sofia"],
  ])("default_login_name_is_sanitized_from_the_email: %s", async (email, expected) => {
    const m = self({ members: () => [sofiaFor("self", { email })] });
    await m.user.click(await screen.findByRole("button", { name: "New login" }));
    const dialog = await screen.findByRole("dialog", { name: "New login" });
    const name = within(dialog).getByLabelText("Login name") as HTMLInputElement;
    expect(name.value).toBe(expected);
    expect(name.value).toMatch(/^[a-z_][a-z0-9_]{0,62}$/);
  });

  it("offers_only_projects_and_databases_she_can_log_in_to_and_creates_the_login", async () => {
    const created = login({ role_name: "sofia_w", database_id: D2 });
    const m = self({ routes: { [`PUT /v1/databases/${D2}/personal-login`]: ok(secret(created), 201) } });
    await m.user.click(await screen.findByRole("button", { name: "New login" }));
    const dialog = await screen.findByRole("dialog", { name: "New login" });
    const project = within(dialog).getByLabelText("Project");
    expect(within(project).getAllByRole("option").map((o) => o.textContent)).toEqual(["Billing app", "Data warehouse"]);
    await m.user.selectOptions(project, P2);
    await m.user.selectOptions(within(dialog).getByLabelText("Database"), D2);
    await m.user.clear(within(dialog).getByLabelText("Login name"));
    await m.user.type(within(dialog).getByLabelText("Login name"), "sofia_w");
    await m.user.click(within(dialog).getByRole("button", { name: "Create login" }));
    const put = (await waitFor(() => {
      const c = calls(m.f, "PUT", "/v1/databases");
      expect(c).toHaveLength(1);
      return c[0];
    }));
    expect(new URL(put.url).pathname).toBe(`/v1/databases/${D2}/personal-login`);
    expect(await put.clone().json()).toEqual({ name: "sofia_w" });
    const shown = await screen.findByRole("dialog", { name: "Your login sofia_w on warehouse" });
    expect(within(shown).getByLabelText("Password")).toHaveValue(PASSWORD);
  });

  it("rejects_a_bad_name_before_asking_the_server", async () => {
    const m = self();
    await m.user.click(await screen.findByRole("button", { name: "New login" }));
    const dialog = await screen.findByRole("dialog", { name: "New login" });
    await m.user.clear(within(dialog).getByLabelText("Login name"));
    await m.user.type(within(dialog).getByLabelText("Login name"), "Bad-Name");
    await m.user.click(within(dialog).getByRole("button", { name: "Create login" }));
    expect(within(dialog).getByText(/lowercase letters, digits and underscores/)).toBeInTheDocument();
    expect(calls(m.f, "PUT", "/v1/databases")).toHaveLength(0);
  });

  it.each([
    [409, "personal_login_exists", "a personal login already exists", /already have a login on billing-prod/],
    [409, "role_exists", "A role named sofia already exists on billing-prod", /A role named sofia already exists/],
    [422, "invalid_role_name", "That login name is reserved", /That login name is reserved/],
    [409, "credentials_unavailable", "Owner credentials are missing for this database", /Owner credentials are missing/],
    [403, "member_suspended", "Your account is suspended", /Your account is suspended/],
  ])("a_%s_%s_is_shown_in_the_dialog_and_nothing_is_revealed", async (status, code, message, shown) => {
    const m = self({ routes: { [`PUT /v1/databases/${D1}/personal-login`]: apiError(status, code, message) } });
    await m.user.click(await screen.findByRole("button", { name: "New login" }));
    const dialog = await screen.findByRole("dialog", { name: "New login" });
    await m.user.click(within(dialog).getByRole("button", { name: "Create login" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(shown);
    expect(calls(m.f, "PUT", "/v1/databases")).toHaveLength(1);
    expect(screen.queryByRole("dialog", { name: /Your login/ })).toBeNull();
    expect(within(dialog).getByRole("button", { name: "Create login" })).toBeEnabled();
  });
});

describe("Personal Postgres logins: the reveal dialog", () => {
  it("the_password_is_shown_once_and_gone_after_close", async () => {
    const m = await reveal();
    const { dialog, user } = m;
    expect(within(dialog).getByLabelText("Password")).toHaveValue(PASSWORD);
    expect(within(dialog).getByLabelText("Connection string · main branch")).toHaveValue(CONNECTION);
    expect(within(dialog).getByText("You won't see this password again. Copy it now.")).toBeInTheDocument();
    expect(within(dialog).getByText(/Your roles here/)).toHaveTextContent(/reports_reader.*etl_writer|etl_writer.*reports_reader/);
    await user.click(within(dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: /Your login/ })).toBeNull());
    expect(screen.queryByDisplayValue(PASSWORD)).toBeNull();
    expect(document.body.textContent).not.toContain(PASSWORD);
    expect(document.body.innerHTML).not.toContain(PASSWORD);
    // Open again: the old password is not kept anywhere to show.
    expect(screen.queryByLabelText("Password")).toBeNull();
  });

  it("escape_closes_it_and_clears_the_password_too", async () => {
    const m = await reveal();
    await m.user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog", { name: /Your login/ })).toBeNull());
    expect(document.body.innerHTML).not.toContain(PASSWORD);
  });

  it("the_password_never_reaches_the_query_cache_or_storage", async () => {
    const m = await reveal();
    // While the dialog is open...
    expect(leaks(m)).not.toContain(PASSWORD);
    await m.user.click(within(m.dialog).getByRole("button", { name: "Done" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: /Your login/ })).toBeNull());
    // ...and after it closes, the list refetch included.
    expect(leaks(m)).not.toContain(PASSWORD);
  });

  it("a_created_login_password_never_reaches_the_caches_either", async () => {
    const created = login({ role_name: "sofia_w", database_id: D1 });
    const m = self({ routes: { [`PUT /v1/databases/${D1}/personal-login`]: ok(secret(created), 201) } });
    await m.user.click(await screen.findByRole("button", { name: "New login" }));
    await m.user.click(within(await screen.findByRole("dialog", { name: "New login" })).getByRole("button", { name: "Create login" }));
    await screen.findByRole("dialog", { name: /Your login/ });
    expect(leaks(m)).not.toContain(PASSWORD);
  });

  it("copies_the_password_and_the_connection_string", async () => {
    const m = await reveal();
    await m.user.click(within(m.dialog).getByRole("button", { name: "Copy password" }));
    expect(await navigator.clipboard.readText()).toBe(PASSWORD);
    await m.user.click(within(m.dialog).getByRole("button", { name: "Copy connection string" }));
    expect(await navigator.clipboard.readText()).toBe(CONNECTION);
  });
});

describe("Personal Postgres logins: the default role (P33)", () => {
  it("none_gives_the_plain_connection_string_and_the_picker_lists_her_groups", async () => {
    const { dialog } = await reveal();
    const picker = within(dialog).getByLabelText("Default role");
    expect(picker).toHaveValue("");
    expect(within(picker).getAllByRole("option").map((o) => o.textContent).sort()).toEqual(["None", "etl_writer", "reports_reader"]);
    expect(within(dialog).getByLabelText("Connection string · main branch")).toHaveValue(CONNECTION);
  });

  it("a_chosen_role_adds_the_url_encoded_options_parameter", async () => {
    const { dialog, user } = await reveal();
    await user.selectOptions(within(dialog).getByLabelText("Default role"), "etl_writer");
    expect(within(dialog).getByLabelText("Connection string · main branch")).toHaveValue(`${CONNECTION}&options=-c%20role%3Detl_writer`);
    // The password box is untouched.
    expect(within(dialog).getByLabelText("Password")).toHaveValue(PASSWORD);
    await user.click(within(dialog).getByRole("button", { name: "Copy connection string" }));
    expect(await navigator.clipboard.readText()).toBe(`${CONNECTION}&options=-c%20role%3Detl_writer`);
    // Back to none: the plain string again.
    await user.selectOptions(within(dialog).getByLabelText("Default role"), "");
    expect(within(dialog).getByLabelText("Connection string · main branch")).toHaveValue(CONNECTION);
  });

  it("a_string_without_a_query_gets_a_question_mark", async () => {
    const plain = `postgresql://sofia_m:${PASSWORD}@ep-warehouse-main.db.northwind.example:5432/warehouse`;
    const { dialog, user } = await reveal({}, secret(LOGIN_LATER, { connection_string: plain }));
    await user.selectOptions(within(dialog).getByLabelText("Default role"), "etl_writer");
    expect(within(dialog).getByLabelText("Connection string · main branch")).toHaveValue(`${plain}?options=-c%20role%3Detl_writer`);
  });

  it("a_group_name_is_url_encoded_whatever_it_holds", async () => {
    const me = meFor("self") as unknown as { projects: { project_id: string; roles: unknown; databases: { database_id: string; roles: unknown }[] }[] };
    const roles = ["etl_writer", "a&b"];
    const patched = {
      ...me,
      projects: me.projects.map((p) => (p.project_id === P2 ? { ...p, roles, databases: p.databases.map((d) => ({ ...d, roles })) } : p)),
    };
    const { dialog, user } = await reveal({ routes: { "GET /v1/me": ok(patched) } });
    await user.selectOptions(within(dialog).getByLabelText("Default role"), "a&b");
    expect(within(dialog).getByLabelText("Connection string · main branch")).toHaveValue(`${CONNECTION}&options=-c%20role%3Da%26b`);
  });
});

describe("Personal Postgres logins: Reset, Renew and Delete", () => {
  it("reset_password_asks_the_server_and_reveals_the_new_password", async () => {
    const m = await reveal({}, secret(LOGIN_LATER, { password: "n3w-P4ssword-Zk9" }));
    expect(within(m.dialog).getByLabelText("Password")).toHaveValue("n3w-P4ssword-Zk9");
    expect(calls(m.f, "POST", "/v1/databases")).toHaveLength(1);
    expect(new URL(calls(m.f, "POST", "/v1/databases")[0].url).pathname).toBe(`/v1/databases/${D2}/personal-login/reset-password`);
  });

  it("a_failed_reset_shows_the_reason_and_no_dialog", async () => {
    const m = self({ routes: { [RESET_D2]: apiError(404, "not_found", "no live login") } });
    await m.user.click(within(await rowOf(/sofia_m/)).getByRole("button", { name: "Reset password" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(/no live login/);
    expect(screen.queryByRole("dialog", { name: /Your login/ })).toBeNull();
  });

  it("renew_appears_within_14_days_and_resets_through_the_reveal_dialog", async () => {
    const m = self({ routes: { [RESET_D1]: ok(secret(LOGIN_SOON, { password: "R3newed-Pw-Xy7" }), 200) } });
    const soon = await rowOf(/sofia.*billing-prod|billing-prod.*sofia/);
    // Renew replaces Reset on a login that expires within 14 days; the later one keeps Reset and has no Renew.
    expect(within(soon).getByRole("button", { name: "Renew" })).toBeInTheDocument();
    expect(within(soon).queryByRole("button", { name: "Reset password" })).toBeNull();
    const later = await rowOf(/sofia_m/);
    expect(within(later).queryByRole("button", { name: "Renew" })).toBeNull();
    expect(within(later).getByRole("button", { name: "Reset password" })).toBeInTheDocument();
    expect(screen.getByText(/sofia on billing-prod expires on/)).toHaveTextContent(/Renew it for 90 more days/);
    await m.user.click(within(soon).getByRole("button", { name: "Renew" }));
    const dialog = await screen.findByRole("dialog", { name: "Your login sofia on billing-prod" });
    expect(within(dialog).getByLabelText("Password")).toHaveValue("R3newed-Pw-Xy7");
    // Renew is the reset route: there is no other.
    expect(calls(m.f, "POST", "/v1/databases").map((r) => new URL(r.url).pathname)).toEqual([`/v1/databases/${D1}/personal-login/reset-password`]);
  });

  it("an_expired_login_offers_renew_too", async () => {
    const expired = login({ role_name: "sofia", database_id: D1, status: "expired", valid_until: daysAgo(2) });
    self({ logins: () => [expired] });
    const row = await rowOf(/sofia/);
    expect(within(row).getByRole("button", { name: "Renew" })).toBeInTheDocument();
    expect(within(row).getByText(/Expired/)).toBeInTheDocument();
  });

  it("a_login_exactly_14_days_out_still_gets_renew_and_15_days_does_not", async () => {
    const edge = login({ role_name: "sofia", database_id: D1, valid_until: inDays(13.9) });
    const far = login({ role_name: "sofia_m", database_id: D2, valid_until: inDays(15) });
    self({ logins: () => [edge, far] });
    expect(within(await rowOf(/billing-prod/)).getByRole("button", { name: "Renew" })).toBeInTheDocument();
    expect(within(await rowOf(/warehouse/)).queryByRole("button", { name: "Renew" })).toBeNull();
  });

  it("delete_asks_then_removes_the_login", async () => {
    let logins: PersonalLoginView[] = ALL();
    const m = self({
      logins: () => logins,
      routes: {
        [`DELETE /v1/databases/${D2}/personal-logins/${SOFIA}`]: () => {
          logins = logins.filter((l) => l.database_id !== D2);
          return new Response(null, { status: 204 });
        },
      },
    });
    await m.user.click(within(await rowOf(/sofia_m/)).getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete the login sofia_m?" });
    // Cancel asks nothing of the server.
    await m.user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
    expect(calls(m.f, "DELETE", "/v1/databases")).toHaveLength(0);
    await m.user.click(within(await rowOf(/sofia_m/)).getByRole("button", { name: "Delete" }));
    await m.user.click(within(await screen.findByRole("alertdialog", { name: "Delete the login sofia_m?" })).getByRole("button", { name: "Delete login" }));
    await waitFor(() => expect(calls(m.f, "DELETE", "/v1/databases")).toHaveLength(1));
    await waitFor(() => expect(screen.queryByRole("row", { name: /sofia_m/ })).toBeNull());
  });

  it("a_tenant_admin_deletes_someone_elses_login_by_member_id", async () => {
    const m = mount({
      caller: "tenant_admin",
      logins: ALL,
      routes: { [`DELETE /v1/databases/${D2}/personal-logins/${SOFIA}`]: () => new Response(null, { status: 204 }) },
    });
    await m.user.click(within(await rowOf(/sofia_m/)).getByRole("button", { name: "Delete" }));
    await m.user.click(within(await screen.findByRole("alertdialog", { name: "Delete the login sofia_m?" })).getByRole("button", { name: "Delete login" }));
    await waitFor(() => expect(calls(m.f, "DELETE", "/v1/databases")).toHaveLength(1));
  });

  it("a_failed_delete_is_shown_in_the_dialog", async () => {
    const m = self({ routes: { [`DELETE /v1/databases/${D2}/personal-logins/${SOFIA}`]: () => jsonResponse({ error: { code: "not_found", message: "no such login" } }, 404) } });
    await m.user.click(within(await rowOf(/sofia_m/)).getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("alertdialog", { name: "Delete the login sofia_m?" });
    await m.user.click(within(dialog).getByRole("button", { name: "Delete login" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(/no such login/);
  });
});
