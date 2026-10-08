import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { BranchRole, ConnectionDetails } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, sequence, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { BranchRoles } from "./branch-roles";

const role = (over: Partial<BranchRole> & { name: string }): BranchRole => ({
  origin: "api",
  level: "plain",
  login: true,
  password: "generated",
  blocked_keys: [],
  pending: false,
  created_at: "2026-10-01T09:00:00Z",
  inherit: true,
  connection_limit: -1,
  valid_until: null,
  bypass_rls: false,
  member_of: [],
  ...over,
});
const owner = role({ name: "chelabase_owner", origin: "owner", level: "owner_level" });
const reader = role({ name: "reporting" });
const admin = role({ name: "ops", level: "owner_level" });
const sqlRole = role({ name: "analyst", origin: "sql", password: "set_in_sql" });
const md5Role = role({ name: "legacy", origin: "sql", password: "md5_unusable" });

const details = (user: string, password: string): ConnectionDetails => ({
  host: "ep-aaaaaaaaaaaa.localtest.me",
  port: 4432,
  database: "neondb",
  user,
  password,
  password_state: "generated",
  ca_file: null,
  http_url: "https://ep-aaaaaaaaaaaa.localtest.me",
  uri: `postgres://${user}:${password}@ep-aaaaaaaaaaaa.localtest.me:4432/neondb`,
});

const ROLES = "/v1/databases/db/branches/br/roles";
const CONNECTION = "/v1/databases/db/branches/br/connection";
const list = (rows: BranchRole[]): FakeHandler => () => jsonResponse(rows);
const connection: FakeHandler = (req) => {
  const r = new URL(req.url).searchParams.get("role") ?? "chelabase_owner";
  if (r === "analyst") {
    return jsonResponse({ ...details(r, ""), password: null, password_state: "set_in_sql" });
  }
  return jsonResponse(details(r, `${r}-password-xyz`));
};
const refused = (status: number, code: string, message: string, extra: object = {}): FakeHandler => () =>
  jsonResponse({ error: { code, message, ...extra } }, status);

function setup(routes: Record<string, FakeHandler> = {}, rows: BranchRole[] = [owner, reader, admin]) {
  const f = fakeFetch({ [`GET ${ROLES}`]: list(rows), [`GET ${CONNECTION}`]: connection, ...routes });
  const result = renderWithQuery(<BranchRoles databaseId="db" branchId="br" />, { fetch: f });
  return { ...result, fetch: f };
}
const sent = (f: FakeFetch) => f.calls.map((r) => `${r.method} ${new URL(r.url).pathname}`);
const btn = (name: string | RegExp) => screen.getByRole("button", { name });
const rowOf = async (name: string) => within(await screen.findByRole("row", { name: new RegExp(name) }));
function cachedState(queryClient: ReturnType<typeof setup>["queryClient"]): string {
  return JSON.stringify([
    queryClient.getQueryCache().findAll().map((q) => q.state),
    queryClient.getMutationCache().getAll().map((m) => m.state),
  ]);
}

describe("BranchRoles", () => {
  it("lists_roles_with_badges", async () => {
    setup({}, [owner, role({ name: "reporting", pending: true }), admin]);
    const r = await rowOf("reporting");
    expect(r.getByText("Plain")).toBeInTheDocument();
    expect(r.getByText("Pending")).toBeInTheDocument();
    expect((await rowOf("ops")).getByText("Owner-level")).toBeInTheDocument();
    expect((await rowOf("chelabase_owner")).getByText("Owner-level")).toBeInTheDocument();
  });

  it("roles_show_origin_level_and_password_state", async () => {
    setup({}, [owner, reader, sqlRole, md5Role]);
    const o = await rowOf("chelabase_owner");
    expect(o.getByText("Owner")).toBeInTheDocument();
    const r = await rowOf("reporting");
    expect(r.getByText("Studio")).toBeInTheDocument();
    expect(r.getByText("Plain")).toBeInTheDocument();
    expect(r.getByText("Generated")).toBeInTheDocument();
    expect(r.getByText("Yes")).toBeInTheDocument();
    const a = await rowOf("analyst");
    expect(a.getByText("SQL")).toBeInTheDocument();
    expect(a.getByText("Password set in SQL — reset to get a revealable one")).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Origin" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Login" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Password" })).toBeInTheDocument();
  });

  it("role_list_shows_new_attributes", async () => {
    const group = role({ name: "readers", login: false, origin: "sql", password: "none" });
    const member = role({
      name: "etl",
      inherit: false,
      connection_limit: 5,
      valid_until: "2030-01-01T00:00:00Z",
      bypass_rls: true,
      member_of: ["readers", "writers"],
    });
    setup({}, [owner, group, member]);
    expect(screen.getByRole("columnheader", { name: "Attributes" })).toBeInTheDocument();
    const m = await rowOf("etl");
    expect(m.getByText("Bypasses RLS")).toBeInTheDocument();
    expect(m.getByText("No inherit")).toBeInTheDocument();
    expect(m.getByText("At most 5 connections")).toBeInTheDocument();
    expect(m.getByText("Valid until 2030-01-01 00:00 UTC")).toBeInTheDocument();
    expect(m.getByText("Member of readers, writers")).toBeInTheDocument();
    // Defaults say nothing; the list stays read-only.
    const g = await rowOf("^readers");
    expect(g.queryByText(/Bypasses RLS|No inherit|connections|Valid until|Member of/)).toBeNull();
    expect(screen.queryByRole("checkbox")).toBeNull();
  });

  it("md5_warning", async () => {
    setup({}, [owner, md5Role]);
    const r = await rowOf("legacy");
    expect(r.getByText("MD5 password: can't log in through Chelabase; reset it")).toBeInTheDocument();
    expect(r.getByRole("button", { name: "Reset password for legacy" })).toBeInTheDocument();
  });

  it("blocked_keys_are_named", async () => {
    setup({}, [owner, role({ name: "ops", level: "owner_level", blocked_keys: [{ id: "k1", name: "ci-key" }, { id: "k2", name: "worker" }] })]);
    expect(await screen.findByText("Data API keys blocked: ci-key, worker")).toBeInTheDocument();
  });

  it("sql_role_reset_offers_revealable_password", async () => {
    setup(
      {
        [`POST ${ROLES}/analyst/reset-password`]: () => new Response(null, { status: 204 }),
        [`GET ${CONNECTION}`]: () => jsonResponse(details("analyst", "analyst-fresh-pw")),
      },
      [owner, sqlRole],
    );
    await userEvent.click((await rowOf("analyst")).getByRole("button", { name: "Reset password for analyst" }));
    await userEvent.click(btn("Confirm reset"));
    expect(await screen.findByText("analyst-fresh-pw")).toBeInTheDocument();
  });

  it("reveal_without_a_password_shows_the_state_text", async () => {
    setup({}, [owner, sqlRole]);
    await userEvent.click((await rowOf("analyst")).getByRole("button", { name: "Reveal analyst" }));
    const section = await screen.findByRole("region", { name: "Connection for analyst" });
    expect(within(section).getAllByText("Password set in SQL — reset to get a revealable one").length).toBe(1);
    expect(within(section).queryByRole("button", { name: "Copy password" })).not.toBeInTheDocument();
  });

  it("role_reveal_shows_ca_file_hint", async () => {
    setup(
      { [`GET ${CONNECTION}`]: () => jsonResponse({ ...details("chelabase_owner", "pw-xyz"), ca_file: ".chelabase/neon/proxy-ca.crt" }) },
      [owner, reader],
    );
    await userEvent.click((await rowOf("chelabase_owner")).getByRole("button", { name: "Reveal chelabase_owner" }));
    const section = await screen.findByRole("region", { name: "Connection for chelabase_owner" });
    expect(within(section).getByText(/Local dev: pass the proxy's CA with/)).toHaveTextContent(
      "sslrootcert=.chelabase/neon/proxy-ca.crt",
    );
  });

  it("polls_while_a_role_is_pending", async () => {
    const f = setup({
      [`GET ${ROLES}`]: sequence(list([owner, role({ name: "reporting", pending: true })]), list([owner, reader])),
    });
    expect(await screen.findByText("Pending")).toBeInTheDocument();
    await vi.waitFor(() => expect(screen.queryByText("Pending")).not.toBeInTheDocument(), { timeout: 4000 });
    expect(sent(f.fetch).filter((s) => s === `GET ${ROLES}`).length).toBeGreaterThanOrEqual(2);
  });

  it("create_plain_by_default_then_reveals_the_password", async () => {
    const created = role({ name: "newbie", pending: true });
    const { fetch } = setup({ [`POST ${ROLES}`]: () => jsonResponse(created, 201) });
    await screen.findByText("reporting");
    await userEvent.type(screen.getByLabelText("Role name"), "newbie");
    expect(screen.getByLabelText(/Plain \(recommended\)/)).toBeChecked();
    await userEvent.click(btn("Create role"));
    expect(await screen.findByText("newbie-password-xyz")).toBeInTheDocument();
    const post = fetch.calls.find((r) => r.method === "POST")!;
    expect(await post.clone().json()).toEqual({ name: "newbie", owner_level: false });
    const get = fetch.calls.find((r) => r.url.includes("/connection?role=newbie"));
    expect(get).toBeDefined();
    expect(screen.getByText("Copy this password now. You can reveal it again later.")).toBeInTheDocument();
  });

  it("create_sends_owner_level", async () => {
    const { fetch } = setup({ [`POST ${ROLES}`]: () => jsonResponse(role({ name: "boss", level: "owner_level" }), 201) });
    await screen.findByText("reporting");
    await userEvent.type(screen.getByLabelText("Role name"), "boss");
    await userEvent.click(screen.getByLabelText(/Owner-level: bypasses RLS and can create roles/));
    await userEvent.click(btn("Create role"));
    await vi.waitFor(() => expect(fetch.calls.some((r) => r.method === "POST")).toBe(true));
    const post = fetch.calls.find((r) => r.method === "POST")!;
    expect(await post.clone().json()).toEqual({ name: "boss", owner_level: true });
  });

  it("delete_confirms_and_shows_keys_on_role_in_use", async () => {
    const del = vi.fn(refused(409, "role_in_use", "role is used by Data API keys", { keys: [{ id: "k1", name: "ci-key" }] }));
    setup({ [`DELETE ${ROLES}/reporting`]: del });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Delete reporting" }));
    expect(screen.getByText(/Objects owned by reporting move to chelabase_owner\. Its sessions are closed\./)).toBeInTheDocument();
    expect(del).not.toHaveBeenCalled();
    await userEvent.click(btn("Confirm delete role"));
    expect(await screen.findByText("ci-key")).toBeInTheDocument();
    expect(screen.getByText(/Data API tab/)).toBeInTheDocument();
  });

  // A role made in SQL may be named anything: the name is one encoded path segment, so
  // "%61" isn't decoded to "a" and "/", "?" and "#" don't change the route.
  it("role_names_are_encoded_in_the_path", async () => {
    const odd = role({ name: "a/b%61?#", origin: "sql", password: "set_in_sql" });
    const encoded = `${ROLES}/a%2Fb%2561%3F%23`;
    const del = vi.fn(() => new Response(null, { status: 204 }));
    const reset = vi.fn(() => new Response(null, { status: 204 }));
    const { fetch } = setup(
      { [`DELETE ${encoded}`]: del, [`POST ${encoded}/reset-password`]: reset },
      [owner, odd],
    );
    await userEvent.click((await rowOf("a/b%61")).getByRole("button", { name: "Reset password for a/b%61?#" }));
    await userEvent.click(btn("Confirm reset"));
    await vi.waitFor(() => expect(reset).toHaveBeenCalled());
    await userEvent.click((await rowOf("a/b%61")).getByRole("button", { name: "Delete a/b%61?#" }));
    await userEvent.click(btn("Confirm delete role"));
    await vi.waitFor(() => expect(del).toHaveBeenCalled());
    const raw = fetch.calls.filter((r) => r.method !== "GET").map((r) => `${r.method} ${new URL(r.url).pathname}`);
    expect(raw).toEqual([`POST ${encoded}/reset-password`, `DELETE ${encoded}`]);
  });

  it("owner_has_no_delete", async () => {
    setup();
    const r = await rowOf("chelabase_owner");
    expect(r.getByRole("button", { name: "Reveal chelabase_owner" })).toBeInTheDocument();
    expect(r.getByRole("button", { name: "Reset password for chelabase_owner" })).toBeInTheDocument();
    expect(r.queryByRole("button", { name: /Delete/ })).not.toBeInTheDocument();
  });

  it("reset_reveals_the_new_password", async () => {
    const { fetch } = setup({ [`POST ${ROLES}/reporting/reset-password`]: () => new Response(null, { status: 204 }) });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reset password for reporting" }));
    await userEvent.click(btn("Confirm reset"));
    expect(await screen.findByText("reporting-password-xyz")).toBeInTheDocument();
    expect(fetch.calls.some((r) => r.url.includes("/connection?role=reporting"))).toBe(true);
  });

  it("reset_not_applied_shows_the_notice", async () => {
    setup({ [`POST ${ROLES}/reporting/reset-password`]: refused(503, "not_applied", "not applied") });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reset password for reporting" }));
    await userEvent.click(btn("Confirm reset"));
    expect(await screen.findByText("The new password is stored but not yet applied; it will be retried.")).toBeInTheDocument();
    expect(await screen.findByText("reporting-password-xyz")).toBeInTheDocument();
  });

  it("create_not_applied_refreshes_and_shows_the_pending_role", async () => {
    const message = "the role was stored but not yet applied; it will be retried";
    setup({
      [`GET ${ROLES}`]: sequence(list([owner, reader]), list([owner, reader, role({ name: "newbie", pending: true })])),
      [`POST ${ROLES}`]: refused(503, "not_applied", message),
    });
    await screen.findByText("reporting");
    await userEvent.type(screen.getByLabelText("Role name"), "newbie");
    await userEvent.click(btn("Create role"));
    expect(await screen.findByRole("alert")).toHaveTextContent(message);
    const r = await rowOf("newbie");
    expect(r.getByText("Pending")).toBeInTheDocument();
  });

  it("a_failed_delete_refreshes_the_list", async () => {
    setup({
      [`GET ${ROLES}`]: sequence(list([owner, reader, admin]), list([owner, admin])),
      [`DELETE ${ROLES}/reporting`]: refused(503, "not_applied", "deleted, not applied yet"),
    });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Delete reporting" }));
    await userEvent.click(btn("Confirm delete role"));
    expect(await screen.findByRole("alert")).toHaveTextContent("deleted, not applied yet");
    await vi.waitFor(() => expect(screen.queryByText("reporting")).not.toBeInTheDocument());
  });

  it("a_failed_reset_refreshes_the_list", async () => {
    const { fetch } = setup({
      [`POST ${ROLES}/reporting/reset-password`]: refused(500, "internal", "internal error"),
    });
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reset password for reporting" }));
    const before = sent(fetch).filter((s) => s === `GET ${ROLES}`).length;
    await userEvent.click(btn("Confirm reset"));
    expect(await screen.findByRole("alert")).toHaveTextContent("internal error");
    await vi.waitFor(() => expect(sent(fetch).filter((s) => s === `GET ${ROLES}`).length).toBeGreaterThan(before));
  });

  it("name_hint_shows_only_after_a_submit_with_a_bad_name", async () => {
    const { fetch } = setup();
    await screen.findByText("reporting");
    expect(screen.queryByText("Enter a name for the role.")).not.toBeInTheDocument();
    await userEvent.click(btn("Create role"));
    const hint = screen.getByText("Enter a name for the role.");
    expect(screen.getByLabelText("Role name")).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByLabelText("Role name")).toHaveAccessibleDescription(hint.textContent!);
    await userEvent.type(screen.getByLabelText("Role name"), "Bad-Name");
    expect(screen.queryByText("Enter a name for the role.")).not.toBeInTheDocument();
    await userEvent.click(btn("Create role"));
    expect(screen.getByText(/lowercase letters, digits and underscores/)).toBeInTheDocument();
    expect(fetch.calls.some((r) => r.method === "POST")).toBe(false);
  });

  it("passwords_never_enter_the_query_cache", async () => {
    const { queryClient } = setup({
      [`POST ${ROLES}`]: () => jsonResponse(role({ name: "newbie" }), 201),
      [`POST ${ROLES}/reporting/reset-password`]: () => new Response(null, { status: 204 }),
    });
    await userEvent.type(await screen.findByLabelText("Role name"), "newbie");
    await userEvent.click(btn("Create role"));
    await screen.findByText("newbie-password-xyz");
    expect(cachedState(queryClient)).not.toContain("newbie-password-xyz");
    await userEvent.click((await rowOf("reporting")).getByRole("button", { name: "Reset password for reporting" }));
    await userEvent.click(btn("Confirm reset"));
    await screen.findByText("reporting-password-xyz");
    expect(cachedState(queryClient)).not.toContain("reporting-password-xyz");
    await userEvent.click((await rowOf("ops")).getByRole("button", { name: "Reveal ops" }));
    await screen.findByText("ops-password-xyz");
    expect(cachedState(queryClient)).not.toContain("password-xyz");
  });
});
