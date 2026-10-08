import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LandingPage } from "./landing-page";
import { forkCount } from "./shore-scene";
import { TideGauge } from "./tide-gauge";

const renderPage = (studioUrl = "https://s.test") => render(<LandingPage studioUrl={studioUrl} />);

describe("LandingPage", () => {
  it("leads_with_the_chelabase_name", () => {
    renderPage();
    expect(screen.getByRole("heading", { level: 1, name: "Chelabase" })).toBeInTheDocument();
  });

  it("ctas_link_to_studio", () => {
    renderPage();
    const starts = screen.getAllByRole("link", { name: "Start free" });
    const logins = screen.getAllByRole("link", { name: "Log in" });
    expect(starts.length).toBeGreaterThanOrEqual(2);
    expect(logins.length).toBeGreaterThanOrEqual(2);
    for (const a of starts) expect(a.getAttribute("href")).toBe("https://s.test/?signup");
    for (const a of logins) expect(a.getAttribute("href")).toBe("https://s.test/");
  });

  it("labels_object_storage_coming_soon", () => {
    renderPage();
    const row = screen.getByRole("listitem", { name: /object storage/i });
    expect(row).toHaveTextContent(/coming soon/i);
    expect(row).toHaveTextContent(/SeaweedFS/);
  });

  it("marks_the_comparison_as_a_preview_without_numbers", () => {
    renderPage();
    const table = screen.getByRole("table", { name: /comparison preview/i });
    for (const name of ["Chelabase", "Supabase", "Firebase", "Vanilla Neon", "PlanetScale"]) {
      expect(within(table).getByRole("columnheader", { name })).toBeInTheDocument();
    }
    // Chelabase's own column is set in the wordmark, claw C included
    expect(within(table).getByRole("columnheader", { name: "Chelabase" }).querySelector("svg")).not.toBeNull();
    for (const cell of within(table).getAllByRole("cell")) expect(cell.textContent).not.toMatch(/\d/);
    expect(screen.getByText(/benchmarks coming later/i)).toBeInTheDocument();
  });

  it("comparison_has_five_columns_with_planetscale_to_survey", () => {
    renderPage();
    const table = screen.getByRole("table", { name: /comparison preview/i });
    const headers = within(table)
      .getAllByRole("columnheader")
      // the wordmark header reads "Chelabase" to screen readers (sr-only) and draws "helabase" after the claw
      .map((h) => h.querySelector(".sr-only")?.textContent ?? h.textContent);
    expect(headers).toEqual(["Chelabase", "Supabase", "Firebase", "Vanilla Neon", "PlanetScale"]);
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(8);
    const cells = rows.map((r) => within(r).getAllByRole("cell")[4]);
    expect(cells.map((c) => c.textContent)).toEqual(Array(8).fill("To survey"));
  });

  it("labels_the_branching_drawings_as_illustrations", () => {
    renderPage();
    expect(screen.getAllByText(/illustration/i).length).toBeGreaterThanOrEqual(2);
  });

  it("shows_the_readme_setup_path", () => {
    renderPage();
    const steps = screen.getByRole("figure", { name: /your terminal/i });
    for (const cmd of ["git clone https://github.com/chelabase/chelabase.git", "proto install", "pnpm install", "just dev"]) {
      expect(steps).toHaveTextContent(cmd);
    }
  });

  it("puts_the_setup_path_before_branching", () => {
    renderPage();
    const setup = screen.getByRole("heading", { level: 2, name: /from clone to a query/i });
    const branching = screen.getByRole("heading", { level: 2, name: /branch it/i });
    expect(setup.compareDocumentPosition(branching) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("walkthrough_step_two_says_open_studio_and_sign_up", () => {
    const { container } = renderPage();
    expect(container.textContent).toContain("Open Studio and sign up. Email verification is off in self-hosted mode.");
    expect(container.textContent).not.toContain("choose Start free");
  });

  it("hero_ends_with_the_merge_label", () => {
    const { container } = renderPage();
    expect(container.textContent).toContain("staging merged into main");
    expect(container.textContent).not.toMatch(/promoted/i);
  });

  it("hero_aria_label_describes_the_merge", () => {
    renderPage();
    const scene = screen.getByRole("img", { name: /crab walks main/i });
    expect(scene).toHaveAccessibleName(/and staging is merged into main, its crab burrowing into main\.$/i);
    expect(scene).not.toHaveAccessibleName(/promot/i);
  });

  it("offers_a_replay_for_the_hero_animation", () => {
    renderPage();
    expect(screen.getByRole("button", { name: /replay/i })).toBeInTheDocument();
  });
});

describe("the hero with reduced motion", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("shows_the_complete_end_state_without_animating", () => {
    const raf = vi.fn();
    vi.stubGlobal("requestAnimationFrame", raf);
    vi.stubGlobal(
      "matchMedia",
      (query: string) => ({ matches: query.includes("reduce"), media: query, addEventListener() {}, removeEventListener() {} }),
    );
    renderPage();
    expect(raf).not.toHaveBeenCalled();
    // main's crab and one crab per branch, all placed at full size
    const scene = screen.getByRole("img", { name: /crab walks main/i });
    const crabs = [...scene.querySelectorAll<SVGGElement>(":scope > g[transform^='translate']")];
    expect(crabs.length).toBe(4);
    for (const crab of crabs) {
      const scale = Number(/scale\(([\d.]+)\)/.exec(crab.getAttribute("transform") ?? "")?.[1]);
      expect(scale).toBeGreaterThan(0.1);
    }
    // the merge note and burrow belong to the animated finish only
    const notes = [...scene.querySelectorAll(":scope > text")].filter((t) => /^(reset|staging merged into main)$/.test(t.textContent ?? ""));
    expect(notes).toHaveLength(2);
    for (const el of [...notes, ...scene.querySelectorAll(":scope > ellipse")]) {
      expect(el.getAttribute("opacity")).toBe("0");
    }
  });
});

describe("the hero at phone width", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("names_the_second_branch_staging_and_merges_it", () => {
    // jsdom has no layout: report a 390px-wide scene so the scene draws two branches
    const w = vi.spyOn(Element.prototype, "clientWidth", "get").mockReturnValue(390);
    const h = vi.spyOn(Element.prototype, "clientHeight", "get").mockReturnValue(360);
    vi.stubGlobal(
      "matchMedia",
      (query: string) => ({ matches: query.includes("reduce"), media: query, addEventListener() {}, removeEventListener() {} }),
    );
    try {
      renderPage();
      const scene = screen.getByRole("img", { name: /crab walks main/i });
      const names = [...scene.querySelectorAll(":scope > text")].map((t) => t.textContent);
      expect(names).toEqual(expect.arrayContaining(["main", "preview", "staging"]));
      expect(names).not.toContain("search");
      expect(names).toContain("staging merged into main");
      // main + two branches
      expect(scene.querySelectorAll(":scope > g[transform^='translate']")).toHaveLength(3);
      expect(scene.querySelectorAll(":scope > ellipse")).toHaveLength(1);
    } finally {
      w.mockRestore();
      h.mockRestore();
    }
  });

  it("keeps_search_between_preview_and_staging_on_desktop", () => {
    const w = vi.spyOn(Element.prototype, "clientWidth", "get").mockReturnValue(1440);
    const h = vi.spyOn(Element.prototype, "clientHeight", "get").mockReturnValue(460);
    try {
      renderPage();
      const scene = screen.getByRole("img", { name: /crab walks main/i });
      const names = [...scene.querySelectorAll(":scope > text")].map((t) => t.textContent);
      expect(names.filter((n) => ["preview", "search", "staging"].includes(n ?? ""))).toEqual(["preview", "search", "staging"]);
    } finally {
      w.mockRestore();
      h.mockRestore();
    }
  });
});

describe("forkCount", () => {
  it("fits_two_branches_on_phones_and_three_in_the_desktop_column", () => {
    expect(forkCount(390)).toBe(2);
    expect(forkCount(1240)).toBe(3);
  });
});

describe("TideGauge", () => {
  it("describes_the_chosen_point_in_time", () => {
    render(<TideGauge />);
    const slider = screen.getByRole("slider", { name: /point in time/i });
    fireEvent.change(slider, { target: { value: "72" } });
    expect(slider).toHaveAttribute("aria-valuetext", "3 days ago");
  });

  it("switches_between_branch_reset_and_restore", async () => {
    render(<TideGauge />);
    await userEvent.click(screen.getByRole("radio", { name: "Restore" }));
    expect(screen.getByRole("radio", { name: "Restore" })).toBeChecked();
    expect(screen.getByText(/before-restore/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("radio", { name: "Reset" }));
    expect(screen.getByRole("slider", { name: /point in time/i })).toBeDisabled();
  });
});
