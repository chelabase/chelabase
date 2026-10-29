import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getGetMeQueryKey, type ConnectionDetails, type DatabaseSummary } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeFetch, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { DatabaseActions } from "./database-actions";

const ready: DatabaseSummary = {
  database_id: "d1",
  name: "web1",
  postgres_database: "web1",
  status: "ready",
  host: "127.0.0.1",
  port: 49153,
  branch_count: 1,
};

const EP = "ep-0123456789ab.localtest.me";
const HTTP_URL = `https://${EP}:4444/sql`;
const uriFor = (password: string) => `postgresql://postgres:${password}@${EP}:4432/web1?sslmode=verify-full`;

const details = (password: string): ConnectionDetails => ({
  host: EP,
  port: 4432,
  database: "web1",
  user: "postgres",
  password,
  uri: uriFor(password),
  http_url: HTTP_URL,
  password_state: "generated",
  ca_file: null,
});

const CONNECTION = "GET /v1/databases/d1/connection";
const RESET = "POST /v1/databases/d1/reset-password";
const DELETE = "DELETE /v1/databases/d1";

const connection = (password: string) => () => jsonResponse(details(password));
const fail = (status: number, code: string, message: string) => () =>
  jsonResponse({ error: { code, message } }, status);
/** The orchestrator's 503 when a reset stored the new password but couldn't apply it yet. */
const notApplied = fail(503, "not_applied", "the password was changed but not yet applied; it will be retried");

function setup(routes: Record<string, FakeHandler>, database: DatabaseSummary = ready) {
  const f = fakeFetch(routes);
  const result = renderWithQuery(<DatabaseActions database={database} />, { fetch: f });
  return { ...result, fetch: f };
}
const count = (f: FakeFetch, key: string) =>
  f.calls.filter((r) => `${r.method} ${new URL(r.url).pathname}` === key).length;

afterEach(() => vi.unstubAllGlobals());

describe("DatabaseActions", () => {
  it("reveals details with the password masked until asked", async () => {
    setup({ [CONNECTION]: connection("s3cret") });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));

    expect(await screen.findByText("postgres")).toBeInTheDocument();
    expect(screen.getByText("web1", { selector: "dd" })).toBeInTheDocument();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(screen.queryByText(/postgres:\/\//)).toBeNull();

    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("s3cret")).toBeInTheDocument();
    expect(screen.getByText(uriFor("s3cret"))).toBeInTheDocument();
  });

  it("copies the connection URI", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup({ [CONNECTION]: connection("s3cret") });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    await userEvent.click(await screen.findByRole("button", { name: "Copy URI" }));
    expect(writeText).toHaveBeenCalledWith(uriFor("s3cret"));
  });

  it("shows_the_http_url_without_the_password", async () => {
    setup({ [CONNECTION]: connection("s3cret") });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    expect(await screen.findByText(HTTP_URL)).toBeInTheDocument();
    expect(screen.getByText("HTTP URL")).toBeInTheDocument();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(screen.queryByText(/postgresql:\/\//)).toBeNull();
  });

  it("copies_the_http_url", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup({ [CONNECTION]: connection("s3cret") });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    await userEvent.click(await screen.findByRole("button", { name: "Copy HTTP URL" }));
    expect(writeText).toHaveBeenCalledWith(HTTP_URL);
  });

  it("connection_shows_ca_file_hint", async () => {
    setup({ [CONNECTION]: () => jsonResponse({ ...details("s3cret"), ca_file: ".chelabase/neon/proxy-ca.crt" }) });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    const note = await screen.findByText(/CA file: pass the proxy's CA with/);
    expect(note).toHaveTextContent(
      "CA file: pass the proxy's CA with sslrootcert=.chelabase/neon/proxy-ca.crt. Self-hosted, fetch it with docker compose cp proxy:/tls/ca.crt .chelabase/neon/proxy-ca.crt (install guide, “Connect to databases”). If localtest.me resolves to ::1 first on your machine, add hostaddr=127.0.0.1.",
    );
  });

  it("no_ca_hint_without_ca_file", async () => {
    setup({ [CONNECTION]: connection("s3cret") });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    await screen.findByText("postgres");
    expect(screen.queryByText(/sslrootcert/)).toBeNull();
    expect(screen.getByText(/hostaddr=127\.0\.0\.1/)).toBeInTheDocument();
  });

  it("forgets revealed details when the panel is closed", async () => {
    const { fetch } = setup({ [CONNECTION]: connection("s3cret") });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    await userEvent.click(await screen.findByRole("button", { name: "Show password" }));
    expect(screen.getByText("s3cret")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    expect(screen.queryByText("s3cret")).toBeNull();

    // Reopening fetches again and starts masked.
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    expect(await screen.findByRole("button", { name: "Show password" })).toBeInTheDocument();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(count(fetch, CONNECTION)).toBe(2);
  });

  it("has a copy button for every field", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup({ [CONNECTION]: connection("s3cret") });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    for (const [field, value] of [
      ["host", EP],
      ["port", "4432"],
      ["database", "web1"],
      ["user", "postgres"],
      ["password", "s3cret"],
      ["HTTP URL", HTTP_URL],
    ]) {
      await userEvent.click(await screen.findByRole("button", { name: `Copy ${field}` }));
      expect(writeText).toHaveBeenLastCalledWith(value);
    }
  });

  it("only offers credentials for ready databases", () => {
    setup({}, { ...ready, status: "provisioning" });
    expect(screen.getByRole("button", { name: "Connection details" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Reset password" })).toBeDisabled();
  });

  it("resets the password after confirmation and shows the new details", async () => {
    const { fetch } = setup({ [RESET]: () => new Response(null, { status: 204 }), [CONNECTION]: connection("n3w") });

    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    expect(count(fetch, RESET)).toBe(0);
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));

    expect(count(fetch, RESET)).toBe(1);
    expect(await screen.findByText(/password was reset/i)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("n3w")).toBeInTheDocument();
  });

  it("shows a reset failure", async () => {
    setup({ [RESET]: fail(504, "timeout", "worker did not respond; try again") });
    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("worker did not respond");
  });

  it("shows one message and the stored password after a reset that isn't applied yet", async () => {
    const { fetch } = setup({ [RESET]: notApplied, [CONNECTION]: connection("n3w") });

    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));

    const notice = "The new password is stored but not yet applied; it will be retried.";
    expect(await screen.findByRole("status")).toHaveTextContent(notice);
    expect(count(fetch, CONNECTION)).toBe(1);
    // Not "reset" beside "not applied": one coherent message.
    expect(screen.queryByText(/password was reset/i)).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    await userEvent.click(await screen.findByRole("button", { name: "Show password" }));
    expect(screen.getByText("n3w")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent(notice);
  });

  it("keeps the not-applied message when the details can't be loaded", async () => {
    setup({ [RESET]: notApplied, [CONNECTION]: fail(500, "internal", "boom") });

    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The new password is stored but not yet applied; it will be retried.");
    expect(alert).toHaveTextContent("boom");
  });

  it("doesn't reveal after other reset failures", async () => {
    const { fetch } = setup({ [RESET]: fail(503, "worker_unavailable", "worker unavailable"), [CONNECTION]: connection("x") });
    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("worker unavailable");
    expect(count(fetch, CONNECTION)).toBe(0);
  });

  it("deletes only after the name is typed", async () => {
    const { fetch, queryClient } = setup({ [DELETE]: () => jsonResponse({ status: "deleting" }, 202) });
    queryClient.setQueryData(getGetMeQueryKey(), { user: { sub: "u", org_id: "o" }, tenant: null });

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    const confirm = screen.getByRole("button", { name: "Delete permanently" });
    expect(confirm).toBeDisabled();

    await userEvent.type(screen.getByLabelText(/type the database name/i), "web");
    expect(confirm).toBeDisabled();
    await userEvent.type(screen.getByLabelText(/type the database name/i), "1");
    expect(confirm).toBeEnabled();

    await userEvent.click(confirm);
    await vi.waitFor(() => expect(queryClient.getQueryState(getGetMeQueryKey())?.isInvalidated).toBe(true));
    expect(count(fetch, DELETE)).toBe(1);
  });

  it("reveal_is_never_cached", async () => {
    const { queryClient } = setup({ [CONNECTION]: connection("s3cret") });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    expect(await screen.findByRole("button", { name: "Show password" })).toBeInTheDocument();
    const keys = queryClient.getQueryCache().findAll().map((q) => JSON.stringify(q.queryKey));
    expect(keys.filter((k) => k.includes("connection"))).toEqual([]);
    expect(JSON.stringify(queryClient.getQueryCache().findAll().map((q) => q.state.data))).not.toContain("s3cret");
  });

  it("reset_not_applied_switches_on_code", async () => {
    // The code decides, not the wording: a reworded not_applied still gets the notice...
    setup({ [RESET]: fail(503, "not_applied", "reworded by the server"), [CONNECTION]: connection("n3w") });
    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "The new password is stored but not yet applied; it will be retried.",
    );
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("treats today's not-applied wording under another code as a failure", async () => {
    const { fetch } = setup({
      [RESET]: fail(503, "unavailable", "the password was changed but not yet applied; it will be retried"),
      [CONNECTION]: connection("n3w"),
    });
    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("not yet applied");
    expect(count(fetch, CONNECTION)).toBe(0);
  });

  it("reveals and resets a branch through the branch endpoints", async () => {
    const f = fakeFetch({
      "POST /v1/databases/d1/branches/b2/reset-password": () => new Response(null, { status: 204 }),
      "GET /v1/databases/d1/branches/b2/connection": connection("br4nch"),
    });
    const branch = {
      id: "b2", name: "dev", parent_branch_id: "d1", kind: "normal", status: "ready", host: EP, port: 4432,
      branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
      protected: false, main_database_missing: false, guard_logins: true, review_acknowledged_hash: null,
    } as const;
    const { queryClient } = renderWithQuery(<DatabaseActions database={ready} branch={branch} />, { fetch: f });
    expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Reset password" }));
    expect(screen.getByText(/Reset the password for dev\?/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Confirm reset" }));
    expect(await screen.findByText(/password was reset/i)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("br4nch")).toBeInTheDocument();
    // The role list may be cached, the password never.
    expect(JSON.stringify(queryClient.getQueryCache().findAll().map((q) => q.state))).not.toContain("br4nch");
  });

  it("connection_picker_switches_role", async () => {
    const branch = {
      id: "b2", name: "dev", parent_branch_id: "d1", kind: "normal", status: "ready", host: EP, port: 4432,
      branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
      protected: false, main_database_missing: false, guard_logins: true, review_acknowledged_hash: null,
    } as const;
    const row = (name: string, kind: "owner" | "login") => ({
      name, origin: kind === "owner" ? "owner" : "api", level: kind === "owner" ? "owner_level" : "plain",
      login: true, password: "generated", blocked_keys: [], pending: false, created_at: "2026-10-01T09:00:00Z",
    } as const);
    const f = fakeFetch({
      "GET /v1/databases/d1/branches/b2/roles": () => jsonResponse([row("chelabase_owner", "owner"), row("reporting", "login")]),
      "GET /v1/databases/d1/branches/b2/connection": (req) => {
        const r = new URL(req.url).searchParams.get("role");
        return jsonResponse({ ...details(r ? "rolepw" : "ownerpw"), user: r ?? "chelabase_owner" });
      },
    });
    renderWithQuery(<DatabaseActions database={ready} branch={branch} />, { fetch: f });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    const picker = await screen.findByRole("combobox", { name: "Role" });
    // Its label is exactly "Role", not "Role" plus its options (what Playwright's
    // getByLabel('Role', { exact: true }) matches against).
    expect(screen.getByLabelText("Role", { exact: true })).toBe(picker);
    const labels = Array.from((picker as HTMLSelectElement).labels ?? []).map((l) => l.textContent);
    expect(labels).toEqual(["Role"]);
    expect(picker).toHaveValue("chelabase_owner");
    expect(await screen.findByText("chelabase_owner", { selector: "dd" })).toBeInTheDocument();
    await userEvent.selectOptions(picker, "reporting");
    expect(await screen.findByText("reporting", { selector: "dd" })).toBeInTheDocument();
    expect(f.calls.some((r) => r.url.includes("/connection?role=reporting"))).toBe(true);
    expect(screen.getByRole("combobox", { name: "Role" })).toHaveValue("reporting");
  });

  const branchB2 = {
    id: "b2", name: "dev", parent_branch_id: "d1", kind: "normal", status: "ready", host: EP, port: 4432,
    branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
    protected: false, main_database_missing: false, guard_logins: true, review_acknowledged_hash: null,
  } as const;
  const ownerRow = {
    name: "chelabase_owner", origin: "owner", level: "owner_level", login: true, password: "generated",
    blocked_keys: [], pending: false, created_at: "2026-10-01T09:00:00Z",
  } as const;

  it("database_picker_refetches: main first, the system database labelled, each change re-fetches", async () => {
    const f = fakeFetch({
      "GET /v1/databases/d1/branches/b2/roles": () => jsonResponse([ownerRow]),
      "GET /v1/databases/d1/branches/b2/databases": () =>
        jsonResponse([
          { name: "postgres", origin: "system", owner: "cloud_admin", main: false },
          { name: "web1", origin: "main", owner: "chelabase_owner", main: true },
          { name: "extra", origin: "sql", owner: "chelabase_owner", main: false },
        ]),
      "GET /v1/databases/d1/branches/b2/connection": (req) =>
        jsonResponse({ ...details("pw"), database: new URL(req.url).searchParams.get("database") ?? "web1" }),
    });
    renderWithQuery(<DatabaseActions database={ready} branch={branchB2} />, { fetch: f });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    const picker = await screen.findByRole("combobox", { name: "Database" });
    await vi.waitFor(() => expect(within(picker).getAllByRole("option")).toHaveLength(3));
    expect(within(picker).getAllByRole("option").map((o) => o.textContent)).toEqual(["web1", "extra", "postgres (System)"]);
    expect(picker).toHaveValue("web1");
    await userEvent.selectOptions(picker, "postgres");
    await vi.waitFor(() => expect(f.calls.some((r) => r.url.includes("database=postgres"))).toBe(true));
    await vi.waitFor(() => expect(screen.getByRole("combobox", { name: "Database" })).toHaveValue("postgres"));
  });

  it("connection shows the password state text when the password is null", async () => {
    const f = fakeFetch({
      "GET /v1/databases/d1/branches/b2/roles": () => jsonResponse([ownerRow]),
      "GET /v1/databases/d1/branches/b2/connection": () =>
        jsonResponse({ ...details("x"), password: null, password_state: "set_in_sql" }),
    });
    renderWithQuery(<DatabaseActions database={ready} branch={branchB2} />, { fetch: f });
    await userEvent.click(screen.getByRole("button", { name: "Connection details" }));
    expect(await screen.findByText("Password set in SQL — reset to get a revealable one")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show password" })).toBeNull();
  });

  it("database_delete_disabled_with_protected_branch", async () => {
    const f = fakeFetch({
      "GET /v1/databases/d1/branches": () => jsonResponse([{ ...branchB2, protected: true }]),
    });
    renderWithQuery(<DatabaseActions database={ready} />, { fetch: f });
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    await vi.waitFor(() => expect(screen.getByRole("button", { name: "Delete permanently" })).toBeDisabled());
    expect(screen.getByText("A branch is protected")).toBeInTheDocument();
    await userEvent.type(screen.getByRole("textbox"), "web1");
    expect(screen.getByRole("button", { name: "Delete permanently" })).toBeDisabled();
  });

  it("branches_list_fetched_only_for_delete_panel", async () => {
    const f = fakeFetch({ "GET /v1/databases/d1/branches": () => jsonResponse([branchB2]) });
    renderWithQuery(<DatabaseActions database={ready} />, { fetch: f });
    const listed = () => count(f, "GET /v1/databases/d1/branches");
    await new Promise((r) => setTimeout(r, 50));
    expect(listed()).toBe(0);
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    await vi.waitFor(() => expect(listed()).toBe(1));
    await userEvent.type(screen.getByRole("textbox"), "web1");
    await vi.waitFor(() => expect(screen.getByRole("button", { name: "Delete permanently" })).toBeEnabled());
  });
});
