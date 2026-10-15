import "@testing-library/jest-dom/vitest";
import { cleanup, configure } from "@testing-library/react";
import { afterEach, vi } from "vitest";

// findBy*/waitFor default to 1 s, which the editor's first renders overran under a loaded machine
// (the full suite in parallel); 5 s keeps failures quick while absorbing the load.
configure({ asyncUtilTimeout: 5_000 });

// `vi.waitFor` is vitest's own poller and ignores the line above: its default is 1 s. The editor's
// dry run is debounced 400 ms and then answered by a fake fetch, so a 1 s wait lost the race under
// load. Same 5 s budget unless a test passes its own.
const waitFor = vi.waitFor.bind(vi);
vi.waitFor = ((cb: Parameters<typeof waitFor>[0], options?: Parameters<typeof waitFor>[1]) =>
  waitFor(cb, typeof options === "number" ? options : { timeout: 5_000, ...options })) as typeof vi.waitFor;

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
