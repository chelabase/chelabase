import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// jsdom doesn't implement scrolling; the router's scroll restoration calls these.
if (typeof window !== "undefined") {
  Object.defineProperty(window, "scrollTo", { value: () => {}, writable: true, configurable: true });
  Object.defineProperty(Element.prototype, "scrollTo", { value: () => {}, writable: true, configurable: true });
}

afterEach(() => cleanup());
