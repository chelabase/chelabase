import "@testing-library/jest-dom/vitest";
import { cleanup, configure } from "@testing-library/react";
import { afterEach } from "vitest";

// findBy*/waitFor default to 1 s, which the editor's first renders overran under a loaded machine
// (the full suite in parallel); 5 s keeps failures quick while absorbing the load.
configure({ asyncUtilTimeout: 5_000 });

// jsdom doesn't implement scrolling; the router's scroll restoration calls these.
if (typeof window !== "undefined") {
  Object.defineProperty(window, "scrollTo", { value: () => {}, writable: true, configurable: true });
  Object.defineProperty(Element.prototype, "scrollTo", { value: () => {}, writable: true, configurable: true });
  // jsdom has no layout for ranges; CodeMirror measures text with them on its next animation frame.
  if (typeof Range !== "undefined" && !("getClientRects" in Range.prototype)) {
    const empty = () => ({ x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, toJSON: () => ({}) });
    Object.defineProperty(Range.prototype, "getClientRects", { value: () => [], writable: true, configurable: true });
    Object.defineProperty(Range.prototype, "getBoundingClientRect", { value: empty, writable: true, configurable: true });
  }
}

afterEach(() => cleanup());
