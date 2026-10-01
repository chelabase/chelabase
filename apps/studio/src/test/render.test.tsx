import { customFetch } from "@cheladb/management-client";
import { QueryClient, useQuery } from "@tanstack/react-query";
import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { fakeFetch, jsonResponse } from "./fake-fetch";
import { renderWithQuery } from "./render";

function Probe() {
  const q = useQuery({ queryKey: ["hello"], queryFn: () => customFetch<{ name: string }>("/hello", { method: "GET" }) });
  if (q.error) return <p>error: {q.error.message}</p>;
  return <p>{q.data ? `hi ${q.data.name}` : "loading"}</p>;
}

describe("renderWithQuery", () => {
  it("renders data fetched through the management client and returns the queryClient", async () => {
    const f = fakeFetch({ "GET /hello": () => jsonResponse({ name: "chela" }) });
    const { queryClient } = renderWithQuery(<Probe />, { fetch: f });
    expect(await screen.findByText("hi chela")).toBeInTheDocument();
    expect(queryClient).toBeInstanceOf(QueryClient);
    expect(f.calls[0]?.headers.get("Authorization")).toBe("Bearer test-token");
    expect(new URL(f.calls[0]!.url).origin).toBe("http://studio.test");
  });

  it("keeps the provider on rerender", async () => {
    const f = fakeFetch({ "GET /hello": () => jsonResponse({ name: "chela" }) });
    const { rerender } = renderWithQuery(<Probe />, { fetch: f });
    expect(await screen.findByText("hi chela")).toBeInTheDocument();
    rerender(<Probe />);
    expect(screen.getByText("hi chela")).toBeInTheDocument();
  });

  it("passes onUnauthorized to the management client", async () => {
    const f = fakeFetch({ "GET /hello": () => jsonResponse({ error: { code: "unauthorized", message: "no" } }, 401) });
    const onUnauthorized = vi.fn(() => true);
    renderWithQuery(<Probe />, { fetch: f, onUnauthorized });
    expect(await screen.findByText("error: no")).toBeInTheDocument();
    expect(onUnauthorized).toHaveBeenCalledTimes(1);
  });

  it("does not retry failures", async () => {
    const f = fakeFetch({});
    renderWithQuery(<Probe />, { fetch: f });
    expect(await screen.findByText(/error: no fake route/)).toBeInTheDocument();
    expect(f.calls).toHaveLength(1);
  });
});
