import { QueryClient } from "@tanstack/react-query";
import { render, renderHook, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useRegisterDevtoolsQueryClient } from "@/lib/devtools-query-client";
import { QueryDevtoolsPlugin, StudioDevtools } from "./studio-devtools";

afterEach(() => vi.unstubAllEnvs());

describe("StudioDevtools", () => {
  it("devtools_render_nothing_outside_dev_builds", () => {
    vi.stubEnv("DEV", false);
    const { container } = render(<StudioDevtools />);
    expect(container).toBeEmptyDOMElement();
  });
});

describe("QueryDevtoolsPlugin", () => {
  it("says_so_when_the_page_has_no_query_client", () => {
    render(<QueryDevtoolsPlugin />);
    expect(screen.getByText("This page has no query client.")).toBeInTheDocument();
  });

  it("shows_the_query_panel_for_the_registered_client", () => {
    const client = new QueryClient();
    const page = renderHook(() => useRegisterDevtoolsQueryClient(client));
    render(<QueryDevtoolsPlugin />);
    expect(screen.queryByText("This page has no query client.")).not.toBeInTheDocument();
    expect(screen.getByTestId("query-devtools-panel")).toBeInTheDocument();
    page.unmount();
  });
});
