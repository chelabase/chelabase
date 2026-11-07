import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { B, BRANCHES, D, PROJECTS, ROLES, meAs, pgDb, type Level } from "@/test/table-editor-fixtures";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { StoragePage, type StorageParams } from "@/components/storage/storage-page";

const PLACE: StorageParams = { projectId: "p1", databaseId: "d1", branchId: "b1" };
const S = `${B}/storage`;
const bucket = (name: string, over: Record<string, unknown> = {}) => ({
  id: `id-${name}`, name, public: false, file_size_limit: null, allowed_mime_types: null, write_once: false, created_at: "2026-11-01T00:00:00Z", ...over,
});
const BUCKETS = [bucket("avatars", { public: true }), bucket("invoices", { file_size_limit: 5 * 1024 * 1024 })];
const file = (name: string, size: number) => ({ name, folder: false, size, etag: "e", content_type: "image/png", updated_at: "2026-11-01T00:00:00Z" });
const listing = (...items: unknown[]): FakeHandler => () => jsonResponse({ items, next_cursor: null });

interface Setup {
  me?: ReturnType<typeof meAs>;
  params?: StorageParams;
  routes?: Record<string, FakeHandler>;
  pg?: unknown[];
}
function setup(level: Level, o: Setup = {}) {
  const f = fakeFetch({
    "GET /v1/me": () => jsonResponse(o.me ?? meAs(level)),
    "GET /v1/projects": () => jsonResponse(PROJECTS.map(({ databases: _d, ...p }) => p)),
    [`GET ${D}`]: () => jsonResponse({ database_id: "d1", name: "orders-db", status: "ready", history_window: 604800 }),
    [`GET ${D}/branches`]: () => jsonResponse(BRANCHES),
    [`GET ${B}/databases`]: () => jsonResponse(o.pg ?? [pgDb("app", true), pgDb("reporting", false, "off")]),
    [`GET ${B}/roles`]: () => jsonResponse(ROLES),
    [`GET ${S}/bucket`]: () => jsonResponse(BUCKETS),
    [`POST ${S}/object/list/avatars`]: listing(file("a.png", 1024), file("b.png", 2048)),
    [`POST ${S}/object/list/invoices`]: listing(),
    ...o.routes,
  });
  const navigate = vi.fn();
  const ui = (p: StorageParams) => <StoragePage params={p} navigate={navigate} />;
  const result = renderWithQuery(ui(o.params ?? PLACE), { fetch: f });
  return { ...result, f, navigate, user: userEvent.setup(), goTo: (p: StorageParams) => result.rerender(ui(p)) };
}
const bucketLists = (f: ReturnType<typeof fakeFetch>) => f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname === `${S}/bucket`);
const roleOf = (r: Request) => new URL(r.url).searchParams.get("role");
const list = () => screen.findByRole("list", { name: "Buckets" });

afterEach(() => {
  localStorage.clear();
  sessionStorage.clear();
});

describe("StoragePage: buckets", () => {
  it("lists_buckets_with_badge_size_and_count_under_the_context_bar", async () => {
    setup("admin");
    const items = within(await list());
    const avatars = await items.findByRole("link", { name: /avatars/ });
    expect(within(avatars).getByText("Public")).toBeInTheDocument();
    await waitFor(() => expect(avatars).toHaveTextContent("2 objects"));
    expect(avatars).toHaveTextContent("3 kB");
    expect(within(items.getByRole("link", { name: /invoices/ })).getByText("Private")).toBeInTheDocument();
    for (const label of ["Project", "Database", "Branch", "Act as"]) expect(screen.getByLabelText(label)).toBeInTheDocument();
    // The storage tab has no Postgres database level: storage runs in the branch's main database.
    expect(screen.queryByLabelText("Postgres db")).toBeNull();
  });

  it("a_partial_place_redirects_and_the_last_place_is_remembered", async () => {
    const { navigate } = setup("admin", { params: { projectId: "p1", databaseId: "d1" } });
    await waitFor(() => expect(navigate).toHaveBeenCalled());
    expect(navigate.mock.calls[0][0]).toMatchObject({ projectId: "p1", databaseId: "d1", branchId: "b1" });
    expect(navigate.mock.calls[0][1]).toEqual({ replace: true });
    navigate.mockClear();
    localStorage.clear();
    const second = setup("admin");
    await list();
    await waitFor(() => expect(JSON.parse(localStorage.getItem("chelabase.studio.storage.last") ?? "null")).toEqual(PLACE));
    second.unmount();
  });

  // #103 AC1: the bare link opens at the last storage place.
  it("the_bare_route_opens_at_the_last_storage_place", async () => {
    localStorage.setItem("chelabase.studio.storage.last", JSON.stringify({ projectId: "p1", databaseId: "d1", branchId: "b2" }));
    const { navigate, goTo } = setup("developer", { params: {}, me: meAs("developer", ["billing_admin"]) });
    // The levels fill as their data arrives: project and database first, then (at that place) the branch.
    await waitFor(() => expect(navigate).toHaveBeenCalledWith({ projectId: "p1", databaseId: "d1" }, { replace: true }));
    goTo({ projectId: "p1", databaseId: "d1" });
    await waitFor(() => expect(navigate).toHaveBeenLastCalledWith({ projectId: "p1", databaseId: "d1", branchId: "b2" }, { replace: true }));
  });

  // #103 AC8
  it("an_empty_branch_explains_buckets_and_offers_create", async () => {
    const { user } = setup("admin", { routes: { [`GET ${S}/bucket`]: () => jsonResponse([]) } });
    expect(await screen.findByText(/A bucket holds files/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Create a bucket" }));
    expect(await screen.findByRole("dialog", { name: "New bucket" })).toBeInTheDocument();
  });

  it("creating_a_bucket_posts_its_settings_and_refetches_the_list", async () => {
    let posted: unknown;
    const { f, user } = setup("admin", {
      routes: {
        [`POST ${S}/bucket`]: async (req) => {
          posted = await req.json();
          return jsonResponse(bucket("docs"), 201);
        },
      },
    });
    await list();
    await user.click(screen.getByRole("button", { name: "New bucket" }));
    const dialog = await screen.findByRole("dialog", { name: "New bucket" });
    await user.type(within(dialog).getByLabelText("Name"), "docs");
    await user.click(within(dialog).getByLabelText("Public"));
    await user.type(within(dialog).getByLabelText("Size limit"), "10");
    await user.type(within(dialog).getByLabelText("Allowed types"), "application/pdf");
    const before = bucketLists(f).length;
    await user.click(within(dialog).getByRole("button", { name: "Create bucket" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "New bucket" })).toBeNull());
    expect(posted).toEqual({ name: "docs", public: true, file_size_limit: 10 * 1024 * 1024, allowed_mime_types: ["application/pdf"] });
    await waitFor(() => expect(bucketLists(f).length).toBeGreaterThan(before));
  });

  it("an_invalid_name_is_explained_before_anything_is_sent", async () => {
    const { f, user } = setup("admin");
    await list();
    await user.click(screen.getByRole("button", { name: "New bucket" }));
    const dialog = await screen.findByRole("dialog", { name: "New bucket" });
    await user.type(within(dialog).getByLabelText("Name"), "My Bucket");
    expect(within(dialog).getByRole("button", { name: "Create bucket" })).toBeDisabled();
    expect(within(dialog).getByText(/lower-case letters/)).toBeInTheDocument();
    expect(f.calls.some((r) => r.method === "POST" && r.url.endsWith("/storage/bucket"))).toBe(false);
  });

  // #103 AC2
  it("a_taken_name_shows_the_servers_message_and_the_dialog_stays_open", async () => {
    const { user } = setup("admin", {
      routes: { [`POST ${S}/bucket`]: () => jsonResponse({ error: { code: "conflict", message: "a bucket named `avatars` already exists" } }, 409) },
    });
    await list();
    await user.click(screen.getByRole("button", { name: "New bucket" }));
    const dialog = await screen.findByRole("dialog", { name: "New bucket" });
    await user.type(within(dialog).getByLabelText("Name"), "avatars");
    await user.click(within(dialog).getByRole("button", { name: "Create bucket" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("a bucket named `avatars` already exists");
    expect(screen.getByRole("dialog", { name: "New bucket" })).toBeInTheDocument();
  });

  it("editing_a_bucket_sends_only_the_changes", async () => {
    let patched: unknown;
    const { user } = setup("admin", {
      params: { ...PLACE, bucket: "invoices" },
      routes: {
        [`PATCH ${S}/bucket/invoices`]: async (req) => {
          patched = await req.json();
          return new Response(null, { status: 204 });
        },
      },
    });
    await user.click(await screen.findByRole("button", { name: "Edit settings" }));
    const dialog = await screen.findByRole("dialog", { name: "Edit invoices" });
    expect(within(dialog).getByLabelText("Size limit")).toHaveValue(5);
    await user.click(within(dialog).getByLabelText("Public"));
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Edit invoices" })).toBeNull());
    expect(patched).toEqual({ public: true });
  });

  it("deleting_an_empty_bucket_leaves_the_bucket_level", async () => {
    let deleted = false;
    const { user, navigate } = setup("admin", {
      params: { ...PLACE, bucket: "invoices" },
      routes: {
        [`DELETE ${S}/bucket/invoices`]: () => {
          deleted = true;
          return new Response(null, { status: 204 });
        },
      },
    });
    await user.click(await screen.findByRole("button", { name: "Delete bucket" }));
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: "Delete invoices" }));
    await waitFor(() => expect(deleted).toBe(true));
    await waitFor(() => expect(navigate).toHaveBeenCalledWith(PLACE));
  });

  // #103 AC3
  it("a_non_empty_bucket_is_refused_with_the_count_shown", async () => {
    const { user } = setup("admin", {
      params: { ...PLACE, bucket: "avatars" },
      routes: {
        [`POST ${S}/object/list/avatars`]: listing(file("a.png", 1), file("b.png", 1), file("c.png", 1)),
        [`DELETE ${S}/bucket/avatars`]: () => jsonResponse({ error: { code: "bucket_not_empty", message: "bucket `avatars` still holds objects; delete them first" } }, 409),
      },
    });
    await user.click(await screen.findByRole("button", { name: "Delete bucket" }));
    const dialog = await screen.findByRole("alertdialog");
    await user.click(within(dialog).getByRole("button", { name: "Delete avatars" }));
    const alert = await within(dialog).findByRole("alert");
    expect(alert).toHaveTextContent("avatars still holds 3 objects");
    expect(alert).toHaveTextContent(/delete them first/);
  });

  // #103 AC4
  it("a_viewer_sees_the_list_without_create_edit_or_delete", async () => {
    setup("viewer", { me: meAs("viewer", ["reporter"]), params: { ...PLACE, bucket: "avatars" } });
    expect(await within(await list()).findByRole("link", { name: /avatars/ })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "avatars" })).toBeInTheDocument();
    for (const name of ["New bucket", "Edit settings", "Delete bucket", "Create a bucket"]) expect(screen.queryByRole("button", { name })).toBeNull();
  });

  it("a_developer_acting_as_a_viewer_level_role_gets_the_viewer_controls", async () => {
    const me = meAs("developer", ["reporter"]);
    (me.projects[0] as Record<string, unknown>).role_levels = { reporter: "viewer" };
    setup("developer", { me, params: { ...PLACE, bucket: "avatars" } });
    expect(await screen.findByRole("heading", { name: "avatars" })).toBeInTheDocument();
    for (const name of ["New bucket", "Edit settings", "Delete bucket"]) expect(screen.queryByRole("button", { name })).toBeNull();
  });

  it("a_developer_gets_the_controls_and_lists_as_their_role", async () => {
    const { f } = setup("developer", { me: meAs("developer", ["billing_admin"]), params: { ...PLACE, bucket: "avatars" } });
    expect(await screen.findByRole("button", { name: "Edit settings" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New bucket" })).toBeInTheDocument();
    expect(roleOf(bucketLists(f).at(-1)!)).toBe("billing_admin");
  });

  // #103 AC5
  it("changing_act_as_refetches_the_bucket_list_as_the_new_role", async () => {
    const { f, user } = setup("admin");
    await within(await list()).findByRole("link", { name: /avatars/ });
    // An Admin starts as the owner: no role is sent.
    expect(bucketLists(f).every((r) => roleOf(r) === null)).toBe(true);
    await user.click(screen.getByLabelText("Act as"));
    await user.click(await screen.findByRole("option", { name: "reporter" }));
    await waitFor(() => expect(bucketLists(f).some((r) => roleOf(r) === "reporter")).toBe(true));
  });

  // #103 AC6
  it("a_database_without_the_data_api_says_storage_needs_it_and_links_to_enabling_it", async () => {
    const { f } = setup("admin", { pg: [pgDb("app", true, "off")] });
    expect(await screen.findByText(/Storage needs the data API/)).toBeInTheDocument();
    const link = screen.getByRole("link", { name: /Enable the data API/ });
    expect(link).toHaveAttribute("href", expect.stringContaining("/app/data-api/p1/d1/b1/app"));
    expect(bucketLists(f)).toHaveLength(0);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("a_data_api_disabled_answer_shows_the_same_message_not_an_error", async () => {
    setup("admin", {
      routes: { [`GET ${S}/bucket`]: () => jsonResponse({ error: { code: "data_api_disabled", message: "the data API is off" } }, 409) },
    });
    expect(await screen.findByText(/Storage needs the data API/)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("a_list_error_shows_the_servers_message", async () => {
    setup("admin", { routes: { [`GET ${S}/bucket`]: () => jsonResponse({ error: { code: "forbidden", message: "acting as it needs Admin" } }, 403) } });
    expect(await screen.findByRole("alert")).toHaveTextContent("acting as it needs Admin");
  });

  it("a_bucket_in_the_url_that_isnt_listed_is_not_found", async () => {
    setup("admin", { params: { ...PLACE, bucket: "gone" } });
    expect(await screen.findByText("Bucket not found")).toBeInTheDocument();
  });
});
