import { render, screen } from "@testing-library/react";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { PRESENCE_COLORS } from "@/lib/realtime/presence";
import { contrast, deltaE, resolve, themes } from "@/lib/realtime/presence-palette-helpers";
import { LIVE_COLOR, LiveBadge } from "./live-badge";

/*
 * RT1 second Playwright pass (2026-10-21), ruling 3: the "Live" dot was seafoam, the same as a seafoam member's avatar.
 *
 * Pins for the coder:
 * - new file `live-badge.tsx` exports `LiveBadge({ status })` (moved out of rows-view.tsx with its PAUSED reasons and the
 *   `LiveStatus` type; rows-view imports it from "./live-badge") and `LIVE_COLOR` (a `var(--token)` or a 6-digit hex);
 * - the badge keeps `data-testid="live-state"` and its text exactly "Live" (live) or "Live updates paused · <reason>";
 * - the dot is `data-testid="live-dot"` with `data-color={LIVE_COLOR}` and no seafoam class;
 * - the non-colour cue is a ring: while live, the dot has `data-cue="ring"` and holds an aria-hidden
 *   `data-testid="live-ring"` (a pulse; its class has `motion-reduce:animate-none`, so under reduced motion it is a
 *   still ring). A ring adds no text, so the text stays exactly "Live". Paused: no ring, no data-cue;
 * - LIVE_COLOR is no PRESENCE_COLORS entry and at least 8 (OKLab x100) from each of them, in both themes, and 3:1 on ink/deep/channel.
 */
const DISTANCE = 8;
const root = join(dirname(fileURLToPath(import.meta.url)), ".");

describe("LiveBadge", () => {
  it("live_says_exactly_Live", () => {
    render(<LiveBadge status={{ live: true }} />);
    expect(screen.getByTestId("live-state")).toHaveTextContent(/^Live$/);
  });

  it("the_dot_carries_LIVE_COLOR_and_is_not_seafoam", () => {
    render(<LiveBadge status={{ live: true }} />);
    const dot = screen.getByTestId("live-dot");
    expect(dot).toHaveAttribute("data-color", LIVE_COLOR);
    expect(dot.className).not.toMatch(/seafoam/);
    expect(dot.outerHTML).not.toMatch(/seafoam/);
  });

  it("live_has_a_ring_as_a_non_colour_cue", () => {
    render(<LiveBadge status={{ live: true }} />);
    const dot = screen.getByTestId("live-dot");
    expect(dot).toHaveAttribute("data-cue", "ring");
    const ring = screen.getByTestId("live-ring");
    expect(dot).toContainElement(ring);
    expect(ring).toHaveAttribute("aria-hidden", "true");
    expect(ring.className).toMatch(/motion-reduce:animate-none/);
    expect(ring).toBeEmptyDOMElement();
  });

  it("paused_has_no_ring_and_keeps_its_reason", () => {
    render(<LiveBadge status={{ live: false, reason: "no_slot" }} />);
    expect(screen.getByTestId("live-state")).toHaveTextContent("Live updates paused · no free slot");
    expect(screen.queryByTestId("live-ring")).toBeNull();
    expect(screen.getByTestId("live-dot")).not.toHaveAttribute("data-cue");
  });

  it("paused_without_a_reason_says_unavailable", () => {
    render(<LiveBadge status={{ live: false }} />);
    expect(screen.getByTestId("live-state")).toHaveTextContent("Live updates paused · unavailable");
  });

  it("rows_view_uses_the_component_instead_of_its_own_seafoam_dot", () => {
    const src = readFileSync(join(root, "rows-view.tsx"), "utf8");
    expect(src).toMatch(/from "\.\/live-badge"/);
    expect(src).not.toMatch(/function LiveBadge/);
    expect(src).not.toMatch(/status\.live \? "bg-seafoam"/);
  });
});

describe("LIVE_COLOR", () => {
  it("is_a_token_or_hex_and_no_presence_entry", () => {
    expect(LIVE_COLOR).toMatch(/^(var\(--[a-z0-9-]+\)|#[0-9a-f]{6})$/i);
    expect(PRESENCE_COLORS).not.toContain(LIVE_COLOR);
  });

  describe.each(themes)("%s", (_name, theme) => {
    it("is_at_least_8_from_every_presence_colour", () => {
      const live = resolve(LIVE_COLOR, theme);
      for (const c of PRESENCE_COLORS) {
        expect(deltaE(live, resolve(c, theme)), `${LIVE_COLOR} vs ${c}`).toBeGreaterThanOrEqual(DISTANCE);
      }
    });

    it("reaches_3_to_1_on_ink_deep_and_channel", () => {
      const live = resolve(LIVE_COLOR, theme);
      for (const g of ["--ink", "--deep", "--channel"]) {
        expect(contrast(live, resolve(`var(${g})`, theme)), `${LIVE_COLOR} on ${g}`).toBeGreaterThanOrEqual(3);
      }
    });
  });
});
