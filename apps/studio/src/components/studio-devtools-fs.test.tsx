import { render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { StudioDevtools } from "./studio-devtools";

/**
 * RT1 Playwright pass, fix F5 (2026-10-21).
 *
 * The dev-only TanStack Devtools floating button sat over the row panel's "Save row" button at 1440x900 (bottom
 * right). Every corner and edge position the library offers (`top-left`, `top-right`, `bottom-left`, `bottom-right`,
 * `middle-left`, `middle-right`) lies over something in Studio: the rail's items and its behaviour menu on the left,
 * the header's account menu top right, the row panel and the grid's scrollbar on the right. Decision (pinned): the
 * trigger button is not drawn (`config.triggerHidden: true`); the devtools stay available on the library's open hotkey
 * (default Control+~), and the panels and plugins are unchanged. Only the config is testable here (the button is drawn
 * by the library); the visual check is the Playwright pass.
 */

const captured = vi.hoisted(() => ({ props: null as null | { config?: Record<string, unknown>; plugins?: { name: string }[] } }));
vi.mock("@tanstack/react-devtools", () => ({
  TanStackDevtools: (props: NonNullable<typeof captured.props>) => {
    captured.props = props;
    return null;
  },
}));

beforeEach(() => {
  captured.props = null;
});
afterEach(() => vi.unstubAllEnvs());

describe("StudioDevtools config", () => {
  it("in_dev_the_floating_trigger_button_is_not_drawn_so_it_cannot_cover_studios_controls", () => {
    vi.stubEnv("DEV", true);
    render(<StudioDevtools />);
    expect(captured.props).not.toBeNull();
    expect(captured.props!.config?.triggerHidden).toBe(true);
  });

  it("the_router_and_query_panels_are_still_registered", () => {
    vi.stubEnv("DEV", true);
    render(<StudioDevtools />);
    expect(captured.props!.plugins?.map((p) => p.name)).toEqual(["TanStack Router", "TanStack Query"]);
  });

  it("outside_dev_builds_the_devtools_are_not_rendered_at_all", () => {
    vi.stubEnv("DEV", false);
    render(<StudioDevtools />);
    expect(captured.props).toBeNull();
  });
});
