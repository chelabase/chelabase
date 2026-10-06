import { render } from "@testing-library/react";
import { EditorView } from "@codemirror/view";
import { describe, expect, it } from "vitest";
import { CodeEditor } from "./code-editor";

describe("CodeEditor", () => {
  it("uses CodeMirror's dark base theme, so the caret, active line and selection suit the dark ground", () => {
    const { container } = render(<CodeEditor initialDoc="SELECT 1" extensions={[]} onChange={() => {}} onRun={() => {}} />);
    const dom = container.querySelector<HTMLElement>(".cm-editor");
    expect(dom).not.toBeNull();
    const view = EditorView.findFromDOM(dom!);
    expect(view?.state.facet(EditorView.darkTheme)).toBe(true);
  });

  it("keeps selected glyphs chalk: the theme's native ::selection rule sets color to var(--foreground), not the global ink", () => {
    render(<CodeEditor initialDoc="SELECT 1" extensions={[]} onChange={() => {}} onRun={() => {}} />);
    const css = Array.from(document.querySelectorAll("style")).map((s) => s.textContent ?? "").join("\n");
    const rules = css.split("}").filter((r) => r.includes(".cm-content ::selection") && r.includes("var(--foreground)"));
    expect(rules.length).toBeGreaterThan(0);
    // The sand tint belongs to CodeMirror's own selection layer only.
    const sandOnNative = css.split("}").filter((r) => r.includes(".cm-content ::selection") && r.includes("242 199 163"));
    expect(sandOnNative).toEqual([]);
  });
});
