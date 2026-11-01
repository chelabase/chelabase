import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Branch, ConnectionDetails as Info, DatabaseSummary } from "@chelabase/management-client";
import { fakeFetch, jsonResponse, type FakeHandler } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { ConnectionDetails } from "./connection-details";

const database: DatabaseSummary = {
  database_id: "d1", name: "web1", postgres_database: "web1", status: "ready", host: "h", port: 4432, branch_count: 2,
};
const branch = (id: string) =>
  ({
    id, name: id, parent_branch_id: null, kind: "normal", status: "ready", host: "h", port: 4432,
    branched_at_time: null, branched_at_lsn: null, created_at: "2026-09-30T10:00:00Z", history_start: null, error: null,
    protected: false, main_database_missing: false, guard_logins: true, review_acknowledged_hash: null,
  }) as Branch;

const EP = "ep-0123456789ab.localtest.me";
const HTTP_URL = `https://${EP}:4444/sql`;
const uriFor = (password: string) => `postgresql://postgres:${password}@${EP}:4432/web1?sslmode=verify-full`;
const details = (password: string, over: Partial<Info> = {}): Info => ({
  host: EP, port: 4432, database: "web1", user: "postgres", password, uri: uriFor(password), http_url: HTTP_URL,
  password_state: "generated", ca_file: null, ...over,
});

const CONNECTION = "GET /v1/databases/d1/connection";
const connection = (password: string) => () => jsonResponse(details(password));

function setup(routes: Record<string, FakeHandler>, ui = <ConnectionDetails database={database} />) {
  const f = fakeFetch(routes);
  return { ...renderWithQuery(ui, { fetch: f }), fetch: f };
}

afterEach(() => vi.unstubAllGlobals());

describe("ConnectionDetails", () => {
  it("shows the details with the password masked until asked", async () => {
    setup({ [CONNECTION]: connection("s3cret") });
    expect(await screen.findByText("postgres")).toBeInTheDocument();
    expect(screen.getByText("web1", { selector: "dd" })).toBeInTheDocument();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(screen.queryByText(/postgres:\/\//)).toBeNull();

    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("s3cret")).toBeInTheDocument();
    expect(screen.getByText(uriFor("s3cret"))).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Hide password" }));
    expect(screen.queryByText("s3cret")).toBeNull();
  });

  it("has a copy button for every field, and copies the URI", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal("navigator", { ...navigator, clipboard: { writeText } });
    setup({ [CONNECTION]: connection("s3cret") });
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
    await userEvent.click(screen.getByRole("button", { name: "Copy URI" }));
    expect(writeText).toHaveBeenLastCalledWith(uriFor("s3cret"));
  });

  it("shows the HTTP URL without the password", async () => {
    setup({ [CONNECTION]: connection("s3cret") });
    expect(await screen.findByText(HTTP_URL)).toBeInTheDocument();
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(screen.queryByText(/postgresql:\/\//)).toBeNull();
  });

  it("shows the CA file hint only when there is a CA file", async () => {
    const { unmount } = setup({ [CONNECTION]: () => jsonResponse(details("s3cret", { ca_file: ".chelabase/neon/proxy-ca.crt" })) });
    const note = await screen.findByText(/CA file: pass the proxy's CA with/);
    expect(note).toHaveTextContent("sslrootcert=.chelabase/neon/proxy-ca.crt");
    expect(note).toHaveTextContent("hostaddr=127.0.0.1");
    unmount();
    setup({ [CONNECTION]: connection("s3cret") });
    await screen.findByText("postgres");
    expect(screen.queryByText(/sslrootcert/)).toBeNull();
    expect(screen.getByText(/hostaddr=127\.0\.0\.1/)).toBeInTheDocument();
  });

  it("never puts the password in the query cache", async () => {
    const { queryClient } = setup({ [CONNECTION]: connection("s3cret") });
    expect(await screen.findByRole("button", { name: "Show password" })).toBeInTheDocument();
    const keys = queryClient.getQueryCache().findAll().map((q) => JSON.stringify(q.queryKey));
    expect(keys.filter((k) => k.includes("connection"))).toEqual([]);
    expect(JSON.stringify(queryClient.getQueryCache().findAll().map((q) => q.state.data))).not.toContain("s3cret");
  });

  it("refetches when the branch or the Postgres database changes, and masks the password again", async () => {
    const f = fakeFetch({
      "GET /v1/databases/d1/branches/main/connection": (req) =>
        jsonResponse(details(new URL(req.url).searchParams.get("database") === "app" ? "main-app-pw" : "main-pw")),
      "GET /v1/databases/d1/branches/dev/connection": () => jsonResponse(details("dev-pw", { host: "dev.host" })),
    });
    const { rerender } = renderWithQuery(<ConnectionDetails database={database} branch={branch("main")} />, { fetch: f });
    await userEvent.click(await screen.findByRole("button", { name: "Show password" }));
    expect(screen.getByText("main-pw")).toBeInTheDocument();

    rerender(<ConnectionDetails database={database} branch={branch("main")} pgDatabase="app" />);
    expect(await screen.findByRole("button", { name: "Show password" })).toBeInTheDocument();
    expect(screen.queryByText("main-pw")).toBeNull();
    expect(screen.queryByText("main-app-pw")).toBeNull();
    expect(f.calls.some((r) => r.url.includes("/branches/main/connection?database=app"))).toBe(true);
    await userEvent.click(screen.getByRole("button", { name: "Show password" }));
    expect(screen.getByText("main-app-pw")).toBeInTheDocument();

    rerender(<ConnectionDetails database={database} branch={branch("dev")} pgDatabase="app" />);
    expect(await screen.findByText("dev.host")).toBeInTheDocument();
    expect(screen.queryByText("main-app-pw")).toBeNull();
    expect(screen.getByRole("button", { name: "Show password" })).toBeInTheDocument();
    await waitFor(() => expect(f.calls.filter((r) => r.url.includes("/connection")).length).toBe(3));
  });

  it("shows the password state text when there is no revealable password", async () => {
    setup({ [CONNECTION]: () => jsonResponse(details("x", { password: null, password_state: "set_in_sql" })) });
    expect(await screen.findByText("Password set in SQL — reset to get a revealable one")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show password" })).toBeNull();
  });

  it("shows a load failure", async () => {
    setup({ [CONNECTION]: () => jsonResponse({ error: { code: "internal", message: "boom" } }, 500) });
    expect(await screen.findByRole("alert")).toHaveTextContent("boom");
  });
});
