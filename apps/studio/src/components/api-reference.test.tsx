import { readFileSync } from "node:fs";
import { render } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AccessTokenContext } from "@/lib/access-token";
import { EnvContext } from "@/lib/env-context";
import type { StudioEnv } from "@/lib/env";

const configurations: any[] = [];
vi.mock("@scalar/api-reference-react", () => ({
  ApiReferenceReact: ({ configuration }: { configuration: unknown }) => {
    configurations.push(configuration);
    return <div data-testid="scalar" />;
  },
}));

import ApiReference, { apiReferenceConfiguration } from "./api-reference";

const env: StudioEnv = {
  issuer: "http://issuer",
  clientId: "c",
  projectId: "p",
  orgId: "o",
  apiUrl: "http://gw.test",
};

const wrap = (token: string | undefined) => (
  <EnvContext.Provider value={env}>
    <AccessTokenContext.Provider value={token}>
      <ApiReference />
    </AccessTokenContext.Provider>
  </EnvContext.Provider>
);

const last = () => configurations[configurations.length - 1];
beforeEach(() => (configurations.length = 0));

describe("ApiReference", () => {
  it("api_reference_uses_the_current_token", () => {
    const { rerender } = render(wrap("token-1"));
    expect(last().authentication.preferredSecurityScheme).toBe("bearer");
    expect(last().authentication.securitySchemes.bearer.token).toBe("token-1");
    rerender(wrap("token-2"));
    expect(last().authentication.securitySchemes.bearer.token).toBe("token-2");
  });

  it("points the document's servers at Studio's API URL", () => {
    render(wrap("t"));
    expect(last().content.servers).toEqual([{ url: "http://gw.test" }]);
    expect(Object.keys(last().content.paths).length).toBeGreaterThan(0);
  });

  it("api_reference_is_offline", () => {
    render(wrap("t"));
    expect(last().telemetry).toBe(false);
    expect(last().withDefaultFonts).toBe(false);
    // Omitting proxyUrl falls back to https://proxy.scalar.com on the web layout; "" means no proxy.
    expect(last().proxyUrl).toBe("");
    expect(last().externalUrls).toEqual({ dashboardUrl: "", registryUrl: "", proxyUrl: "", apiBaseUrl: "" });
    expect(last().agent).toEqual({ disabled: true });
    expect(last().mcp).toEqual({ disabled: true });
    expect(last().showDeveloperTools).toBe("never");
    expect(last()).not.toHaveProperty("url");
  });

  it("forces Scalar's dark mode and hides its toggle", () => {
    const config = apiReferenceConfiguration("http://gw", "t") as any;
    expect(config.forceDarkModeState).toBe("dark");
    expect(config.hideDarkModeToggle).toBe(true);
  });

  it("defaults the code samples to JavaScript / Axios", () => {
    const config = apiReferenceConfiguration("http://gw", "t") as any;
    expect(config.defaultHttpClient).toEqual({ targetKey: "js", clientKey: "axios" });
  });

  it("themes Scalar onto the lagoon tokens, scoped to the page wrapper", () => {
    const { container } = render(wrap("t"));
    const wrapper = container.querySelector(".studio-api");
    expect(wrapper).not.toBeNull();
    expect(wrapper!.querySelector('[data-testid="scalar"]')).not.toBeNull();
    // Both stylesheets render as <style> next to the wrapper (vitest inlines CSS as "", so the
    // override's text is read from its file).
    expect(container.querySelectorAll("style")).toHaveLength(2);
    const css = readFileSync("src/components/api-reference.css", "utf8");
    expect(css).toContain("--scalar-background-1: var(--ink)");
    expect(css).toContain("--scalar-color-accent: var(--sand)");
    expect(css).toContain("--scalar-color-red: var(--coral)");
    expect(css).toContain("--scalar-font: var(--font-sans)");
    // Every rule of the override is under .studio-api, or Scalar's teleported popovers
    // (`body > .scalar-app`); its selectors never start elsewhere.
    const selectors = css
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .split("{")
      .slice(0, -1)
      .map((chunk) => chunk.split("}").pop()!.trim());
    expect(selectors.length).toBeGreaterThan(0);
    for (const selector of selectors) {
      for (const part of selector.split(",")) {
        const p = part.trim();
        expect(p.startsWith(".studio-api") || p === "body > .scalar-app").toBe(true);
      }
    }
    expect(css).toContain("body > .scalar-app");
  });

  it("removes the color-mode classes Scalar leaves on document.body when unmounted", () => {
    const { unmount } = render(wrap("t"));
    document.body.classList.add("dark-mode");
    unmount();
    expect(document.body.classList.contains("dark-mode")).toBe(false);
    expect(document.body.classList.contains("light-mode")).toBe(false);
  });

  it("keeps the configuration stable while the token is unchanged", () => {
    const { rerender } = render(wrap("t"));
    rerender(wrap("t"));
    expect(new Set(configurations).size).toBe(1);
  });
});
