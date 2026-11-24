import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { cspNonce, editorNonce } from "./csp-nonce";

function withMeta(attrs: Record<string, string> | null) {
  document.head.querySelectorAll('meta[property="csp-nonce"]').forEach((m) => m.remove());
  if (!attrs) return;
  const meta = document.createElement("meta");
  meta.setAttribute("property", "csp-nonce");
  for (const [k, v] of Object.entries(attrs)) meta.setAttribute(k, v);
  document.head.append(meta);
}

afterEach(() => withMeta(null));

describe("cspNonce", () => {
  it("reads the templated nonce", () => {
    withMeta({ nonce: "9b667bcf-a266-44fc-a238-5ee3ac17b551" });
    expect(cspNonce()).toBe("9b667bcf-a266-44fc-a238-5ee3ac17b551");
  });

  it("is undefined without the meta", () => {
    expect(cspNonce()).toBeUndefined();
  });

  it("ignores the untemplated shell (dev server, vite preview)", () => {
    withMeta({ nonce: "{{placeholder `http.request.uuid`}}" });
    expect(cspNonce()).toBeUndefined();
  });

  it("ignores an empty nonce", () => {
    withMeta({ nonce: "" });
    expect(cspNonce()).toBeUndefined();
  });

  it("matches the shell's own meta (index.html)", () => {
    const html = readFileSync(resolve(__dirname, "../../index.html"), "utf8");
    // The Caddyfile's template writes {http.request.uuid} here; the selector must find this meta.
    expect(html).toContain('<meta property="csp-nonce" nonce="{{placeholder `http.request.uuid`}}" />');
  });
});

describe("editorNonce", () => {
  it("gives CodeMirror the nonce", () => {
    withMeta({ nonce: "abc-123" });
    const state = EditorState.create({ extensions: [editorNonce()] });
    expect(state.facet(EditorView.cspNonce)).toBe("abc-123");
  });

  it("adds nothing without a nonce", () => {
    const state = EditorState.create({ extensions: [editorNonce()] });
    expect(state.facet(EditorView.cspNonce)).toBe("");
  });
});
