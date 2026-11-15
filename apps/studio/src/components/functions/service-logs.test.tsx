import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { fakeFetch, jsonResponse } from "@/test/fake-fetch";
import { renderWithQuery } from "@/test/render";
import { ServiceLogs } from "./service-logs";

const LOGS = "GET /v1/databases/d1/branches/b1/functions/echo/logs";

afterEach(() => vi.unstubAllGlobals());

describe("ServiceLogs", () => {
  it("developer_reads_the_last_lines", async () => {
    const f = fakeFetch({ [LOGS]: () => new Response("listening on 8080\nGET /hi 200\n", { status: 200, headers: { "Content-Type": "text/plain" } }) });
    vi.stubGlobal("fetch", f);
    renderWithQuery(<ServiceLogs databaseId="d1" branchId="b1" name="echo" />, { fetch: f });
    expect(await screen.findByText(/listening on 8080/)).toBeInTheDocument();
    expect(screen.getByText(/GET \/hi 200/)).toBeInTheDocument();
  });

  it("viewer_sees_the_403_reason", async () => {
    const f = fakeFetch({ [LOGS]: () => jsonResponse({ error: { code: "forbidden", message: "reading a service's logs needs Developer" } }, 403) });
    vi.stubGlobal("fetch", f);
    renderWithQuery(<ServiceLogs databaseId="d1" branchId="b1" name="echo" />, { fetch: f });
    expect(await screen.findByRole("alert")).toHaveTextContent("reading a service's logs needs Developer");
  });

  it("refresh_reads_again", async () => {
    const user = userEvent.setup();
    let n = 0;
    const f = fakeFetch({ [LOGS]: () => new Response(`read ${++n}\n`, { status: 200 }) });
    vi.stubGlobal("fetch", f);
    renderWithQuery(<ServiceLogs databaseId="d1" branchId="b1" name="echo" />, { fetch: f });
    await screen.findByText(/read 1/);
    await user.click(screen.getByRole("button", { name: "Refresh logs" }));
    expect(await screen.findByText(/read 2/)).toBeInTheDocument();
  });

  it("empty_logs_say_so", async () => {
    const f = fakeFetch({ [LOGS]: () => new Response("", { status: 200 }) });
    vi.stubGlobal("fetch", f);
    renderWithQuery(<ServiceLogs databaseId="d1" branchId="b1" name="echo" />, { fetch: f });
    expect(await screen.findByText("No log lines yet.")).toBeInTheDocument();
  });
});
