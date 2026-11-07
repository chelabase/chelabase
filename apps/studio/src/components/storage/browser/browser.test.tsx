import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { B, BRANCHES, D, PROJECTS, ROLES, meAs, pgDb, type Level } from "@/test/table-editor-fixtures";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { StoragePage, type StorageParams } from "@/components/storage/storage-page";

const PLACE: StorageParams = { projectId: "p1", databaseId: "d1", branchId: "b1", bucket: "avatars" };
const S = `${B}/storage`;
const LIST = `POST ${S}/object/list/avatars`;
const bucket = (name: string, over: Record<string, unknown> = {}) => ({
  id: `id-${name}`, name, public: false, file_size_limit: null, allowed_mime_types: null, write_once: false, created_at: "2026-11-01T00:00:00Z", ...over,
});
const file = (name: string, size = 1024, content_type = "image/png") => ({ name, folder: false, size, etag: `etag-${name}`, content_type, updated_at: 1762000000 });
const folder = (name: string) => ({ name, folder: true, size: null, etag: null, content_type: null, updated_at: null });

interface Setup {
  me?: ReturnType<typeof meAs>;
  params?: StorageParams;
  routes?: Record<string, FakeHandler>;
  items?: unknown[];
  buckets?: unknown[];
}
function setup(level: Level, o: Setup = {}) {
  // jsdom has no layout: give the virtualised list a screen to fill (as the table editor's tests do).
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue({ x: 0, y: 0, top: 0, left: 0, right: 1200, bottom: 800, width: 1200, height: 800, toJSON: () => ({}) } as DOMRect);
  vi.spyOn(HTMLElement.prototype, "offsetHeight", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "offsetWidth", "get").mockReturnValue(1200);
  const f = fakeFetch({
    "GET /v1/me": () => jsonResponse(o.me ?? meAs(level)),
    "GET /v1/projects": () => jsonResponse(PROJECTS.map(({ databases: _d, ...p }) => p)),
    [`GET ${D}`]: () => jsonResponse({ database_id: "d1", name: "orders-db", status: "ready", history_window: 604800 }),
    [`GET ${D}/branches`]: () => jsonResponse(BRANCHES),
    [`GET ${B}/databases`]: () => jsonResponse([pgDb("app", true)]),
    [`GET ${B}/roles`]: () => jsonResponse(ROLES),
    [`GET ${S}/bucket`]: () => jsonResponse(o.buckets ?? [bucket("avatars", { public: true })]),
    [LIST]: () => jsonResponse({ items: o.items ?? [folder("photos/"), file("a.png"), file("b.png"), file("c.png")], next_cursor: null }),
    ...o.routes,
  });
  const navigate = vi.fn();
  const ui = (p: StorageParams) => <StoragePage params={p} navigate={navigate} apiUrl="https://api.test" />;
  const result = renderWithQuery(ui(o.params ?? PLACE), { fetch: f });
  return { ...result, f, navigate, user: userEvent.setup(), goTo: (p: StorageParams) => result.rerender(ui(p)) };
}
const lists = async (f: ReturnType<typeof fakeFetch>) =>
  Promise.all(f.calls.filter((r) => r.method === "POST" && new URL(r.url).pathname === `${S}/object/list/avatars`).map((r) => r.clone().json() as Promise<Record<string, unknown>>));
/** The browser's own listings (the bucket list's count reads one without a delimiter). */
const browserLists = async (f: ReturnType<typeof fakeFetch>) => (await lists(f)).filter((b) => b.delimiter === "/");
const objects = () => screen.findByRole("list", { name: /^Objects in/ });

afterEach(() => {
  vi.restoreAllMocks();
  localStorage.clear();
  sessionStorage.clear();
});

describe("ObjectBrowser", () => {
  // #104 AC1
  it("a_folder_url_lists_its_contents_with_a_matching_breadcrumb", async () => {
    const { f, user, navigate } = setup("admin", { params: { ...PLACE, path: "photos/2024" }, items: [file("photos/2024/x.png")] });
    const rows = within(await objects());
    expect(await rows.findByRole("button", { name: "x.png" })).toBeInTheDocument();
    expect((await browserLists(f))[0]).toMatchObject({ prefix: "photos/2024/" });
    const crumbs = within(screen.getByRole("navigation", { name: "Folder" }));
    expect(crumbs.getAllByRole("link").map((a) => a.textContent)).toEqual(["avatars", "photos", "2024"]);
    expect(crumbs.getByRole("link", { name: "2024" })).toHaveAttribute("aria-current", "page");
    expect(crumbs.getByRole("link", { name: "photos" })).toHaveAttribute("href", "/app/storage/p1/d1/b1/avatars/photos");
    await user.click(crumbs.getByRole("link", { name: "photos" }));
    expect(navigate).toHaveBeenLastCalledWith({ projectId: "p1", databaseId: "d1", branchId: "b1", bucket: "avatars", path: "photos" });
    await user.click(crumbs.getByRole("link", { name: "avatars" }));
    expect(navigate).toHaveBeenLastCalledWith({ projectId: "p1", databaseId: "d1", branchId: "b1", bucket: "avatars" });
  });

  it("opening_a_folder_navigates_into_it", async () => {
    const { user, navigate } = setup("admin");
    await user.click(await within(await objects()).findByRole("link", { name: "photos" }));
    expect(navigate).toHaveBeenLastCalledWith({ ...PLACE, path: "photos" });
  });

  // #104 AC7
  it("a_new_sort_or_search_lists_from_page_one", async () => {
    const { f, user } = setup("admin", {
      routes: {
        [LIST]: async (req) => {
          const body = (await req.clone().json()) as { cursor?: string };
          return jsonResponse({ items: body.cursor ? [file("z.png")] : [file("a.png")], next_cursor: body.cursor ? null : "more" });
        },
      },
    });
    await within(await objects()).findByRole("button", { name: "a.png" });
    await user.selectOptions(screen.getByLabelText("Sort"), "size:desc");
    // The first listing of each new query is page one (the list then fills the screen by cursor).
    const firstWith = async (want: Record<string, unknown>) => (await browserLists(f)).find((b) => Object.entries(want).every(([k, v]) => b[k] === v));
    await waitFor(async () => expect(await firstWith({ sort_by: "size", order: "desc" })).toBeDefined());
    expect((await firstWith({ sort_by: "size", order: "desc" }))!.cursor ?? null).toBeNull();
    await user.type(screen.getByLabelText("Search objects"), "inv");
    await waitFor(async () => expect(await firstWith({ search: "inv", sort_by: "size" })).toBeDefined());
    expect((await firstWith({ search: "inv", sort_by: "size" }))!.cursor ?? null).toBeNull();
  });

  it("the_view_toggle_is_remembered", async () => {
    const { user, unmount } = setup("admin");
    await objects();
    await user.click(screen.getByRole("button", { name: "Grid view" }));
    expect(screen.getByRole("button", { name: "Grid view" })).toHaveAttribute("aria-pressed", "true");
    unmount();
    setup("admin");
    await objects();
    expect(screen.getByRole("button", { name: "Grid view" })).toHaveAttribute("aria-pressed", "true");
  });

  // #104 AC3
  it("three_selected_objects_are_deleted_after_confirming_the_count", async () => {
    const deleted: string[] = [];
    const del: FakeHandler = (req) => {
      deleted.push(decodeURIComponent(new URL(req.url).pathname.split("/").at(-1)!));
      return new Response(null, { status: 204 });
    };
    const { user } = setup("admin", {
      routes: { [`DELETE ${S}/object/avatars/a.png`]: del, [`DELETE ${S}/object/avatars/b.png`]: del, [`DELETE ${S}/object/avatars/c.png`]: del },
    });
    const rows = within(await objects());
    for (const n of ["a.png", "b.png", "c.png"]) await user.click(await rows.findByRole("checkbox", { name: `Select ${n}` }));
    await user.click(screen.getByRole("button", { name: "Delete 3 objects" }));
    const dialog = await screen.findByRole("alertdialog");
    expect(dialog).toHaveTextContent("Delete 3 objects?");
    await user.click(within(dialog).getByRole("button", { name: "Delete 3 objects" }));
    await waitFor(() => expect(deleted.sort()).toEqual(["a.png", "b.png", "c.png"]));
    expect(await screen.findByText("Deleted 3 objects.")).toBeInTheDocument();
  });

  it("when_one_delete_fails_it_is_reported_by_name_and_the_rest_are_deleted", async () => {
    const ok: FakeHandler = () => new Response(null, { status: 204 });
    const { user } = setup("admin", {
      routes: {
        [`DELETE ${S}/object/avatars/a.png`]: ok,
        [`DELETE ${S}/object/avatars/b.png`]: () => jsonResponse({ error: { code: "referenced", message: "public.users.avatar references it" } }, 409),
        [`DELETE ${S}/object/avatars/c.png`]: ok,
      },
    });
    const rows = within(await objects());
    for (const n of ["a.png", "b.png", "c.png"]) await user.click(await rows.findByRole("checkbox", { name: `Select ${n}` }));
    await user.click(screen.getByRole("button", { name: "Delete 3 objects" }));
    await user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Delete 3 objects" }));
    const report = await screen.findByRole("alert");
    expect(report).toHaveTextContent("Deleted 2 objects; 1 couldn't be deleted.");
    expect(within(report).getByRole("listitem")).toHaveTextContent(/b\.png.*references/);
    // The one that failed stays selected.
    expect(screen.getByRole("button", { name: "Delete 1 object" })).toBeInTheDocument();
  });

  // #104 AC4
  it("a_rename_onto_an_existing_name_shows_the_servers_conflict_and_changes_nothing", async () => {
    let moved: unknown;
    const { f, user } = setup("admin", {
      routes: {
        [`POST ${S}/object/move`]: async (req) => {
          moved = await req.json();
          return jsonResponse({ error: { code: "conflict", message: "the object `b.png` already exists" } }, 409);
        },
      },
    });
    await user.click(await within(await objects()).findByRole("button", { name: "a.png" }));
    const details = await screen.findByRole("complementary", { name: "Object details" });
    await user.click(within(details).getByRole("button", { name: "Rename" }));
    const dialog = await screen.findByRole("dialog", { name: "Rename a.png" });
    const input = within(dialog).getByLabelText("Name");
    await user.clear(input);
    await user.type(input, "b.png");
    const before = (await browserLists(f)).length;
    await user.click(within(dialog).getByRole("button", { name: "Rename" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("the object `b.png` already exists");
    expect(moved).toEqual({ bucket: "avatars", from: "a.png", to: "b.png" });
    expect(screen.getByRole("dialog", { name: "Rename a.png" })).toBeInTheDocument();
    // Nothing changed, so nothing is listed again.
    expect((await browserLists(f)).length).toBe(before);
  });

  it("a_rename_keeps_the_object_in_its_folder", async () => {
    let moved: unknown;
    const { user } = setup("admin", {
      params: { ...PLACE, path: "photos" },
      items: [file("photos/a.png")],
      routes: {
        [`POST ${S}/object/move`]: async (req) => {
          moved = await req.json();
          return new Response(null, { status: 204 });
        },
      },
    });
    await user.click(await within(await objects()).findByRole("button", { name: "a.png" }));
    await user.click(within(await screen.findByRole("complementary", { name: "Object details" })).getByRole("button", { name: "Rename" }));
    const dialog = await screen.findByRole("dialog", { name: "Rename a.png" });
    await user.clear(within(dialog).getByLabelText("Name"));
    await user.type(within(dialog).getByLabelText("Name"), "z.png");
    await user.click(within(dialog).getByRole("button", { name: "Rename" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Rename a.png" })).toBeNull());
    expect(moved).toEqual({ bucket: "avatars", from: "photos/a.png", to: "photos/z.png" });
  });

  it("details_show_size_type_etag_and_the_public_url", async () => {
    const { user } = setup("admin");
    await user.click(await within(await objects()).findByRole("button", { name: "a.png" }));
    const details = within(await screen.findByRole("complementary", { name: "Object details" }));
    expect(details.getByText("1 kB")).toBeInTheDocument();
    expect(details.getByText("image/png")).toBeInTheDocument();
    expect(details.getByText("etag-a.png")).toBeInTheDocument();
    expect(details.getByRole("button", { name: "Copy public URL" })).toBeInTheDocument();
  });

  it("a_signed_url_is_minted_with_the_ttl_picked_and_at_most_an_hour", async () => {
    let signed: unknown;
    const { user } = setup("admin", {
      routes: {
        [`POST ${S}/object/sign/avatars/a.png`]: async (req) => {
          signed = await req.json();
          return jsonResponse({ signed_url: "/storage/v1/object/sign/avatars/a.png?token=t", expires_at: 1762003600 });
        },
      },
      items: [file("a.png", 10, "application/octet-stream")],
    });
    await user.click(await within(await objects()).findByRole("button", { name: "a.png" }));
    await user.click(within(await screen.findByRole("complementary", { name: "Object details" })).getByRole("button", { name: "Copy signed URL" }));
    const dialog = await screen.findByRole("dialog", { name: "Signed URL for a.png" });
    const ttl = within(dialog).getByLabelText("Expires after");
    expect([...(ttl as HTMLSelectElement).options].map((o) => Number(o.value)).every((v) => v <= 3600)).toBe(true);
    await user.selectOptions(ttl, "300");
    await user.click(within(dialog).getByRole("button", { name: "Create URL" }));
    expect(await within(dialog).findByDisplayValue("https://api.test/storage/v1/object/sign/avatars/a.png?token=t")).toBeInTheDocument();
    expect(signed).toEqual({ expires_in: 300 });
  });

  // #104 AC6
  it("a_viewer_gets_no_mutating_actions", async () => {
    const { user } = setup("viewer", { me: meAs("viewer", ["reporter"]) });
    const rows = within(await objects());
    await user.click(await rows.findByRole("button", { name: "a.png" }));
    const details = within(await screen.findByRole("complementary", { name: "Object details" }));
    expect(details.getByRole("button", { name: "Copy path" })).toBeInTheDocument();
    for (const name of ["Rename", "Move", "Copy", "Delete", "Copy signed URL"]) expect(details.queryByRole("button", { name })).toBeNull();
    expect(rows.queryByRole("checkbox")).toBeNull();
    expect(screen.queryByRole("button", { name: /^Delete/ })).toBeNull();
  });

  it("a_developer_gets_the_mutating_actions", async () => {
    const { user } = setup("developer", { me: meAs("developer", ["billing_admin"]) });
    await user.click(await within(await objects()).findByRole("button", { name: "a.png" }));
    const details = within(await screen.findByRole("complementary", { name: "Object details" }));
    for (const name of ["Rename", "Move", "Copy", "Delete", "Copy signed URL", "Download"]) expect(details.getByRole("button", { name })).toBeInTheDocument();
  });

  it("a_write_once_bucket_explains_a_refused_delete", async () => {
    const { user } = setup("admin", {
      buckets: [bucket("avatars", { write_once: true })],
      routes: { [`DELETE ${S}/object/avatars/a.png`]: () => jsonResponse({ error: { code: "write_once", message: "write-once" } }, 409) },
    });
    await user.click(await within(await objects()).findByRole("button", { name: "a.png" }));
    await user.click(within(await screen.findByRole("complementary", { name: "Object details" })).getByRole("button", { name: "Delete" }));
    await user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Delete 1 object" }));
    expect(await screen.findByText(/The bucket is write-once/)).toBeInTheDocument();
  });
});

/** Records each XHR upload and answers it 200. */
function fakeXhr() {
  const sent: { method: string; url: string; headers: Record<string, string>; body: Blob }[] = [];
  class FakeXhr {
    upload: { onprogress: ((e: { loaded: number }) => void) | null } = { onprogress: null };
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    onabort: (() => void) | null = null;
    status = 0;
    responseText = "";
    private req = { method: "", url: "", headers: {} as Record<string, string> };
    open(method: string, url: string) {
      this.req.method = method;
      this.req.url = url;
    }
    setRequestHeader(k: string, v: string) {
      this.req.headers[k.toLowerCase()] = v;
    }
    send(body: Blob) {
      sent.push({ ...this.req, body });
      queueMicrotask(() => {
        this.upload.onprogress?.({ loaded: body.size });
        this.status = 200;
        this.responseText = "{}";
        this.onload?.();
      });
    }
    abort() {
      this.onabort?.();
    }
  }
  vi.stubGlobal("XMLHttpRequest", FakeXhr);
  return sent;
}

describe("ObjectBrowser uploads", () => {
  // #105 AC7
  it("a_viewer_gets_no_upload_buttons_and_a_read_only_drop_zone", async () => {
    setup("viewer", { me: meAs("viewer", ["reporter"]) });
    const list = await objects();
    expect(screen.queryByRole("button", { name: "Upload" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Upload folder" })).toBeNull();
    fireEvent.dragEnter(list, { dataTransfer: { types: ["Files"], files: [], items: [] } });
    expect(await screen.findByText(/read-only/)).toBeInTheDocument();
  });

  // #105 AC1, with the listing refreshed after
  it("a_chosen_file_uploads_into_the_open_folder_as_the_role_and_the_listing_refreshes", async () => {
    const sent = fakeXhr();
    const { f, user } = setup("admin", { params: { ...PLACE, path: "photos" }, items: [] });
    await screen.findByText(/This folder is empty/);
    const before = (await browserLists(f)).length;
    const input = screen.getByLabelText("Files to upload");
    await user.upload(input, new File(["hello"], "c.txt", { type: "text/plain" }));
    const tray = within(await screen.findByRole("region", { name: "Uploads" }));
    expect(await tray.findByText("Uploaded")).toBeInTheDocument();
    expect(sent).toHaveLength(1);
    expect(sent[0]!.method).toBe("POST");
    expect(new URL(sent[0]!.url).pathname).toBe(`${S}/object/avatars/${encodeURIComponent("photos/c.txt")}`);
    expect(sent[0]!.headers["content-type"]).toBe("text/plain");
    expect(sent[0]!.headers.authorization).toBe("Bearer test-token");
    await waitFor(async () => expect((await browserLists(f)).length).toBeGreaterThan(before));
    vi.unstubAllGlobals();
  });
});

describe("ObjectBrowser: a Viewer reads through the download route", () => {
  it("a_viewer_previews_an_image_and_a_text_file_and_gets_no_signed_url", async () => {
    let n = 0;
    Object.assign(URL, { createObjectURL: vi.fn(() => `blob:v-${++n}`), revokeObjectURL: vi.fn() });
    const { f, user } = setup("viewer", {
      me: meAs("viewer", ["reporter"]),
      items: [file("a.png"), file("notes.txt", 11, "text/plain")],
      routes: {
        [`GET ${S}/object/avatars/a.png`]: () => new Response(new Uint8Array([137, 80]), { headers: { "content-type": "image/png" } }),
        [`GET ${S}/object/avatars/notes.txt`]: () => new Response("hello notes", { status: 206, headers: { "content-type": "text/plain" } }),
      },
    });
    const rows = within(await objects());
    await user.click(await rows.findByRole("button", { name: "a.png" }));
    let details = within(await screen.findByRole("complementary", { name: "Object details" }));
    expect(await details.findByRole("img", { name: "a.png" })).toHaveAttribute("src", expect.stringMatching(/^blob:v-/));
    expect(details.getByRole("button", { name: "Download" })).toBeInTheDocument();
    expect(details.queryByRole("button", { name: "Copy signed URL" })).toBeNull();
    const downloads = () => f.calls.filter((r) => r.method === "GET" && new URL(r.url).pathname.startsWith(`${S}/object/avatars/`));
    expect(new URL(downloads()[0].url).searchParams.get("role")).toBe("reporter");
    // Nothing was signed.
    expect(f.calls.some((r) => new URL(r.url).pathname.includes("/object/sign/"))).toBe(false);

    await user.click(rows.getByRole("button", { name: "notes.txt" }));
    details = within(await screen.findByRole("complementary", { name: "Object details" }));
    expect(await details.findByText("hello notes")).toBeInTheDocument();
    expect(downloads().at(-1)!.headers.get("range")).toBe("bytes=0-2097151");
    expect(details.queryByRole("button", { name: "Copy signed URL" })).toBeNull();
  });

  it("a_viewer_downloads_through_the_download_route", async () => {
    Object.assign(URL, { createObjectURL: vi.fn(() => "blob:dl"), revokeObjectURL: vi.fn() });
    const clicked: string[] = [];
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
      clicked.push(`${this.getAttribute("href")} ${this.download}`);
    });
    const { f, user } = setup("viewer", {
      me: meAs("viewer", ["reporter"]),
      items: [file("data.bin", 3, "application/octet-stream")],
      routes: { [`GET ${S}/object/avatars/data.bin`]: () => new Response(new Uint8Array([1, 2, 3]), { headers: { "content-type": "application/octet-stream" } }) },
    });
    await user.click(await within(await objects()).findByRole("button", { name: "data.bin" }));
    await user.click(within(await screen.findByRole("complementary", { name: "Object details" })).getByRole("button", { name: "Download" }));
    await waitFor(() => expect(clicked).toEqual(["blob:dl data.bin"]));
    const get = f.calls.find((r) => r.method === "GET" && new URL(r.url).pathname === `${S}/object/avatars/data.bin`)!;
    expect(get.headers.get("range")).toBeNull();
  });
});
