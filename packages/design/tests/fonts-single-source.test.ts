import { readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { customProperties, readCss } from "./css";
import { fontViolations } from "./fonts";

const apps = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "apps");

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    if (name === "node_modules") return [];
    const path = join(dir, name);
    return statSync(path).isDirectory() ? walk(path) : [path];
  });
}

/** Every css/ts/tsx source of every app, tests excluded. */
function appSources(): string[] {
  return readdirSync(apps)
    .map((app) => join(apps, app, "src"))
    .filter((src) => {
      try {
        return statSync(src).isDirectory();
      } catch {
        return false;
      }
    })
    .flatMap(walk)
    .filter((f) => /\.(css|ts|tsx)$/.test(f) && !/\.test\./.test(f));
}

const stripComments = (s: string) => s.replace(/\/\*[\s\S]*?\*\//g, "");

describe("one font source (D8)", () => {
  it("scans some sources", () => {
    expect(appSources().length).toBeGreaterThan(10);
  });

  it("no_app_imports_fonts", () => {
    const hits = appSources().filter((f) => readFileSync(f, "utf8").includes("@fontsource"));
    expect(hits).toEqual([]);
  });

  it("no_app_defines_font_stacks", () => {
    const hits: string[] = [];
    for (const f of appSources()) {
      for (const hit of fontViolations(readFileSync(f, "utf8"))) hits.push(`${f}: ${hit}`);
    }
    expect(hits).toEqual([]);
  });

  it("no_app_depends_on_fontsource", () => {
    const hits = readdirSync(apps)
      .map((app) => join(apps, app, "package.json"))
      .filter((p) => {
        try {
          return statSync(p).isFile();
        } catch {
          return false;
        }
      })
      .filter((p) => readFileSync(p, "utf8").includes("@fontsource"));
    expect(hits).toEqual([]);
  });

  it("the_package_ships_the_landing_set", () => {
    const tokens = readCss("tokens.css");
    const imports = [...stripComments(tokens).matchAll(/@import\s+"([^"]+)"/g)].map((m) => m[1]);
    expect(imports.sort()).toEqual(
      [
        "@fontsource-variable/atkinson-hyperlegible-next",
        "@fontsource-variable/jetbrains-mono",
        "@fontsource/spectral/400-italic.css",
        "@fontsource/spectral/400.css",
        "@fontsource/spectral/600.css",
        "@fontsource/spectral/700.css",
      ].sort(),
    );
    const props = customProperties(readCss("tailwind.css"));
    expect(props.get("--font-sans")).toBe(
      '"Atkinson Hyperlegible Next Variable", "Atkinson Hyperlegible", system-ui, sans-serif',
    );
    expect(props.get("--font-serif")).toBe('Spectral, "Iowan Old Style", Georgia, serif');
    expect(props.get("--font-mono")).toBe('"JetBrains Mono Variable", ui-monospace, monospace');
  });
});
