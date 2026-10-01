import { QueryClient } from "@tanstack/react-query";
import { renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useDevtoolsQueryClient, useRegisterDevtoolsQueryClient } from "./devtools-query-client";

afterEach(() => vi.unstubAllEnvs());

describe("devtools query client", () => {
  it("registers_the_page_query_client_while_mounted", () => {
    const client = new QueryClient();
    const seen = renderHook(() => useDevtoolsQueryClient());
    expect(seen.result.current).toBeNull();
    const page = renderHook(() => useRegisterDevtoolsQueryClient(client));
    seen.rerender();
    expect(seen.result.current).toBe(client);
    page.unmount();
    seen.rerender();
    expect(seen.result.current).toBeNull();
  });

  it("registers_nothing_outside_dev_builds", () => {
    vi.stubEnv("DEV", false);
    const client = new QueryClient();
    renderHook(() => useRegisterDevtoolsQueryClient(client));
    const seen = renderHook(() => useDevtoolsQueryClient());
    expect(seen.result.current).toBeNull();
  });
});
