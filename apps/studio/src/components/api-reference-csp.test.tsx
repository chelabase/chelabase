// The API reference under the shipped CSP (#306): it renders the real Scalar component (the other
// api-reference test mocks it) and checks that every <style> element and style attribute it
// creates is one the policy in infra/images/studio.Caddyfile allows. A Scalar upgrade that changes
// one fails here with the hash to put in the Caddyfile.
import { createHash } from "node:crypto";
import { render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AccessTokenContext } from "@/lib/access-token";
import { EnvContext } from "@/lib/env-context";
import { directives, policyTemplate } from "@/test/studio-csp";

const NONCE = "0d9e6c1a-5f0b-4c55-9d43-0c2f1e7a9b31";
const sha256 = (text: string) => `'sha256-${createHash("sha256").update(text, "utf8").digest("base64")}'`;

const policy = directives(policyTemplate());
const styleHashes = policy.get("style-src")!.filter((s) => s.startsWith("'sha256-"));
const attrHashes = policy.get("style-src-attr")!.filter((s) => s.startsWith("'sha256-"));

let styleAttrs: string[] = [];
// A <style> is checked when it is inserted and again when its text changes: sonner inserts an
// empty one, then fills it. Each element's text at insertion is recorded with its nonce.
let insertedStyles: { nonce: string | null; text: string }[] = [];
const setAttribute = Element.prototype.setAttribute;
const appendChild = Node.prototype.appendChild;
const insertBefore = Node.prototype.insertBefore;
const recordStyle = (node: Node) => {
  if (node instanceof HTMLStyleElement) insertedStyles.push({ nonce: node.getAttribute("nonce"), text: node.textContent ?? "" });
};

beforeEach(() => {
  const meta = document.createElement("meta");
  meta.setAttribute("property", "csp-nonce");
  meta.setAttribute("nonce", NONCE);
  document.head.append(meta);
  styleAttrs = [];
  insertedStyles = [];
  vi.spyOn(Node.prototype, "appendChild").mockImplementation(function <T extends Node>(this: Node, node: T): T {
    recordStyle(node);
    return appendChild.call(this, node) as T;
  });
  vi.spyOn(Node.prototype, "insertBefore").mockImplementation(function <T extends Node>(this: Node, node: T, child: Node | null): T {
    recordStyle(node);
    return insertBefore.call(this, node, child) as T;
  });
  vi.spyOn(Element.prototype, "setAttribute").mockImplementation(function (this: Element, name: string, value: string) {
    if (name.toLowerCase() === "style") styleAttrs.push(value);
    return setAttribute.call(this, name, value);
  });
});

afterEach(() => {
  vi.restoreAllMocks();
  document.head.querySelectorAll('meta[property="csp-nonce"]').forEach((m) => m.remove());
});

describe("ApiReference under Studio's CSP", () => {
  it("creates only style elements and attributes the policy allows", async () => {
    // Imported here, after the spies: sonner and Scalar's toasts insert their <style> on import.
    const { default: ApiReference } = await import("./api-reference");
    render(
      <EnvContext.Provider value={{ apiUrl: "http://gw.test", studioHost: "studio.test" }}>
        <AccessTokenContext.Provider value="t">
          <ApiReference doc="management" />
        </AccessTokenContext.Provider>
      </EnvContext.Provider>,
    );
    // Scalar mounts its Vue app after React commits; its theme <style> is the last to appear.
    await vi.waitFor(() => expect([...document.querySelectorAll("style")].some((s) => s.textContent?.includes("@layer scalar-theme {"))).toBe(true), {
      timeout: 15_000,
    });

    const styles = [...document.querySelectorAll("style")];
    const withNonce = styles.filter((s) => s.getAttribute("nonce") === NONCE);
    // Studio's own two (Scalar's stylesheet and the design palette over it) carry the nonce.
    expect(withNonce.length).toBe(2);
    const texts = [
      ...styles.map((s) => ({ nonce: s.getAttribute("nonce"), text: s.textContent ?? "" })),
      ...insertedStyles,
    ];
    const unhashed = texts
      .filter((s) => s.nonce !== NONCE)
      .map((s) => ({ hash: sha256(s.text), starts: s.text.slice(0, 60) }))
      .filter((s) => !styleHashes.includes(s.hash));
    expect(unhashed, "a <style> without the nonce whose hash isn't in the Caddyfile's style-src").toEqual([]);

    const attrs = styleAttrs.map((v) => ({ value: v, hash: sha256(v) })).filter((a) => !attrHashes.includes(a.hash));
    expect(attrs, "a style attribute whose hash isn't in the Caddyfile's style-src-attr").toEqual([]);
  });
});
