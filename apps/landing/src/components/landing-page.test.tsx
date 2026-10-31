import { fireEvent, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LandingPage } from "./landing-page";
import { forkCount } from "./shore-scene";
import { TideGauge } from "./tide-gauge";
import { GITHUB_URL } from "@/site-urls";

const DOCS = "https://docs.test";
const renderPage = () => render(<LandingPage docsUrl={DOCS} />);

const COLUMNS = ["Chelabase", "Supabase", "Firebase", "Neon", "PlanetScale Postgres", "PlanetScale MySQL"];

describe("LandingPage", () => {
  it("each_studio_section_shows_its_screenshots_with_alt_text", () => {
    renderPage();
    for (const [id, srcs] of [
      ["manage", ["manage-table-editor", "manage-rls-warning"]],
      ["collaborate", ["collaborate-teams"]],
      ["iterate", ["iterate-branches"]],
    ] as const) {
      const section = document.getElementById(id)!;
      const imgs = within(section).getAllByRole("img");
      for (const src of srcs) {
        const img = imgs.find((i) => i.getAttribute("src") === `/screens/${src}.webp`);
        expect(img, `${id}: ${src}`).toBeDefined();
        expect(img!.getAttribute("alt")!.trim().length).toBeGreaterThan(20);
        expect(img!.getAttribute("loading")).toBe("lazy");
        expect(img!.closest("figure")!.querySelector("figcaption")!.textContent!.trim()).not.toBe("");
      }
    }
  });

  it("leads_with_the_chelabase_name", () => {
    renderPage();
    expect(screen.getByRole("heading", { level: 1, name: "Chelabase" })).toBeInTheDocument();
  });

  it("ctas_link_to_the_quickstart_and_github", () => {
    renderPage();
    const starts = screen.getAllByRole("link", { name: "Get started" });
    const githubs = screen.getAllByRole("link", { name: "GitHub" });
    expect(starts.length).toBeGreaterThanOrEqual(3);
    expect(githubs.length).toBeGreaterThanOrEqual(3);
    for (const a of starts) expect(a.getAttribute("href")).toBe(`${DOCS}/docs/using/quickstart`);
    for (const a of githubs) expect(a.getAttribute("href")).toBe(GITHUB_URL);
  });

  it("get_started_falls_back_to_the_quickstart_mdx_without_a_docs_url", () => {
    render(<LandingPage />);
    for (const a of screen.getAllByRole("link", { name: "Get started" })) {
      expect(a.getAttribute("href")).toBe(`${GITHUB_URL}/blob/main/apps/docs/content/docs/using/quickstart.mdx`);
    }
  });

  it("sections_follow_the_agreed_order", () => {
    renderPage();
    const titles = screen.getAllByRole("heading", { level: 2 }).map((h) => h.textContent);
    expect(titles).toEqual([
      "Run it on your machine.",
      "Everything in one place.",
      "How it compares.",
      "Manage it from one console.",
      "Bring your team in.",
      "Build your app on a typed API.",
      "Branch it. Reset it. Restore it. Merge it.",
      "Start with the Quickstart.",
    ]);
  });

  it("the_platform_cards_lead_with_databases_studio_auth_and_realtime_and_link_to_their_docs", () => {
    renderPage();
    const section = screen.getByRole("region", { name: "Everything in one place." });
    const names = within(section).getAllByRole("heading", { level: 3 }).map((h) => h.textContent?.replace("Coming soon", ""));
    expect(names.slice(0, 4)).toEqual(["Databases", "Studio", "Auth", "Realtime"]);
    expect(names.slice(-2)).toEqual(["CLI", "Object storage"]);
    const gateway = within(section).getByRole("link", { name: /^Gateway/ });
    expect(gateway.getAttribute("href")).toBe(`${DOCS}/docs/architecture/gateway`);
    expect(within(section).getAllByRole("link")).toHaveLength(names.length - 2);
  });

  it("labels_the_cli_and_object_storage_coming_soon_without_links", () => {
    renderPage();
    for (const name of ["CLI", "Object storage"]) {
      const card = screen.getByRole("heading", { level: 3, name: new RegExp(`^${name}`) }).parentElement!;
      expect(card.tagName).not.toBe("A");
      expect(card).toHaveTextContent(/coming soon/i);
    }
    expect(screen.getByRole("heading", { level: 3, name: /^Object storage/ }).parentElement).toHaveTextContent(/SeaweedFS/);
  });

  it("marks_the_comparison_as_a_preview_without_numbers", () => {
    renderPage();
    const table = screen.getByRole("table", { name: /comparison preview/i });
    // Chelabase's own column is set in the wordmark, claw C included
    expect(within(table).getByRole("columnheader", { name: "Chelabase" }).querySelector("svg")).not.toBeNull();
    for (const cell of within(table).getAllByRole("cell")) expect(cell.textContent).not.toMatch(/\d/);
    expect(screen.getByText(/benchmarks coming later/i)).toBeInTheDocument();
  });

  it("comparison_columns_split_planetscale_and_every_cell_is_surveyed", () => {
    renderPage();
    const table = screen.getByRole("table", { name: /comparison preview/i });
    const headers = within(table)
      .getAllByRole("columnheader")
      // the wordmark header reads "Chelabase" to screen readers (sr-only) and draws "helabase" after the claw
      .map((h) => h.querySelector(".sr-only")?.textContent ?? h.textContent);
    expect(headers).toEqual(COLUMNS);
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(12);
    for (const r of rows) expect(within(r).getAllByRole("cell")).toHaveLength(COLUMNS.length);
    expect(table.textContent).not.toMatch(/to survey/i);
    const merge = rows.find((r) => within(r).getByRole("rowheader").textContent === "Schema merge between branches")!;
    expect(within(merge).getAllByRole("cell").map((c) => c.textContent)).toEqual(["Yes", "Partly", "No", "No", "No", "Yes"]);
  });

  it("labels_the_branching_drawings_as_illustrations", () => {
    renderPage();
    expect(screen.getAllByText(/illustration/i).length).toBeGreaterThanOrEqual(2);
  });

  it("shows_the_docker_compose_install", () => {
    renderPage();
    const steps = screen.getByRole("figure", { name: /your terminal/i });
    for (const cmd of [
      "git clone https://github.com/chelabase/chelabase.git",
      "cd chelabase/infra/compose",
      "cp .env.example .env",
      "docker compose up -d --wait",
      "docker compose logs init-secrets",
    ]) {
      expect(steps).toHaveTextContent(cmd);
    }
    expect(steps).not.toHaveTextContent("just dev");
  });

  it("the_install_terminal_mentions_the_example_overlay", () => {
    renderPage();
    const steps = screen.getByRole("figure", { name: /your terminal/i });
    expect(steps).toHaveTextContent("docker-compose.example.yml");
  });

  it("the_client_snippet_uses_the_server_client", () => {
    renderPage();
    const snippet = screen.getByRole("figure", { name: /your server, with @chelabase\/client/i });
    expect(snippet).toHaveTextContent('from "@chelabase/client/server"');
    expect(snippet).toHaveTextContent("createServerClient<Database>({ url, key, types, cookies })");
  });

  it("hero_ends_with_the_schema_merge_label", () => {
    const { container } = renderPage();
    expect(container.textContent).toContain("staging's schema merged into main");
    expect(container.textContent).not.toMatch(/promoted/i);
  });

  it("hero_aria_label_describes_the_schema_merge", () => {
    renderPage();
    const scene = screen.getByRole("img", { name: /crab walks main/i });
    expect(scene).toHaveAccessibleName(/and staging's schema is merged into main, its crab burrowing into the sand\.$/i);
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
    const notes = [...scene.querySelectorAll(":scope > text")].filter((t) => /^(reset|staging's schema merged into main)$/.test(t.textContent ?? ""));
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
      expect(names).toContain("staging's schema merged into main");
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

  it("merge_needs_preview_to_have_branched_two_days_ago", async () => {
    render(<TideGauge />);
    const slider = screen.getByRole("slider", { name: /point in time/i });
    fireEvent.change(slider, { target: { value: "12" } });
    expect(slider).toHaveAttribute("aria-valuetext", "12 hours ago");
    await userEvent.click(screen.getByRole("radio", { name: "Merge" }));
    expect(slider).toHaveAttribute("aria-valuetext", "2 days ago");
    // the staff keeps all seven days; a later pick below two days is clamped
    expect(slider).toHaveAttribute("min", "0");
    fireEvent.change(slider, { target: { value: "6" } });
    expect(slider).toHaveAttribute("aria-valuetext", "2 days ago");
    expect(screen.getByText(/rows never move/)).toBeInTheDocument();
    // back on branch, the pick made before the merge clamp stays
    await userEvent.click(screen.getByRole("radio", { name: "Branch" }));
    expect(slider).toHaveAttribute("aria-valuetext", "2 days ago");
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
