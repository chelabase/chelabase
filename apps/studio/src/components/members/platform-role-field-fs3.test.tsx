import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { PlatformRoleField } from "./platform-role-field";

/** Fix S3 (finish review round 2, item 4): the platform-role control below sm is a grid that doesn't wrap. */

describe("platform role control (fix S3)", () => {
  it("is_a_two_by_two_full_width_grid_below_sm_with_nowrap_labels", () => {
    render(<PlatformRoleField value="developer" onChange={vi.fn()} noAccess={{ selected: false, onSelect: vi.fn() }} />);
    const group = screen.getByRole("radiogroup", { name: "Platform role" });
    expect(group.className).toMatch(/(^|\s)grid(\s|$)/);
    expect(group.className).toMatch(/(^|\s)grid-cols-2(\s|$)/);
    expect(group.className).toMatch(/(^|\s)w-full(\s|$)/);
    expect(group.className).not.toMatch(/(^|\s)inline-flex(\s|$)/);
    const radios = screen.getAllByRole("radio");
    expect(radios).toHaveLength(4);
    for (const r of radios) expect(r.className).toMatch(/whitespace-nowrap/);
  });

  it("without_no_access_the_three_roles_share_the_width", () => {
    render(<PlatformRoleField value="developer" onChange={vi.fn()} />);
    const group = screen.getByRole("radiogroup", { name: "Platform role" });
    expect(group.className).toMatch(/(^|\s)grid-cols-3(\s|$)/);
    expect(group.className).toMatch(/(^|\s)w-full(\s|$)/);
  });
});
