import { vi } from "vitest";

/**
 * jsdom has no layout: this fakes what `Breadcrumbs` measures. The `<nav>` is `nav` px wide and the
 * trail needs `need` px (the last crumb, `lastClient` px wide, reports the shortfall as truncation). `fire` runs the
 * observers' callbacks, as a ResizeObserver would after a resize. `restore` puts everything back.
 */
export function mockTrailLayout(initial: { nav: number; need: number; lastClient?: number }) {
  const state = { lastClient: 50, ...initial };
  const observers = new Set<() => void>();
  const proto = HTMLElement.prototype;
  const client = Object.getOwnPropertyDescriptor(proto, "clientWidth");
  const scroll = Object.getOwnPropertyDescriptor(proto, "scrollWidth");
  Object.defineProperty(proto, "clientWidth", {
    configurable: true,
    get(this: HTMLElement) {
      if (this.tagName === "NAV") return state.nav;
      return this.getAttribute("aria-current") === "page" ? state.lastClient : 50;
    },
  });
  Object.defineProperty(proto, "scrollWidth", {
    configurable: true,
    get(this: HTMLElement) {
      return this.getAttribute("aria-current") === "page" ? state.lastClient + Math.max(0, state.need - state.nav) : 50;
    },
  });
  const original = globalThis.ResizeObserver;
  class FakeObserver {
    private cb: () => void;
    constructor(cb: () => void) {
      this.cb = cb;
    }
    observe() {
      observers.add(this.cb);
    }
    disconnect() {
      observers.delete(this.cb);
    }
    unobserve() {}
  }
  vi.stubGlobal("ResizeObserver", FakeObserver);
  return {
    set(next: Partial<typeof state>) {
      Object.assign(state, next);
    },
    fire() {
      for (const cb of [...observers]) cb();
    },
    restore() {
      if (client) Object.defineProperty(proto, "clientWidth", client);
      if (scroll) Object.defineProperty(proto, "scrollWidth", scroll);
      vi.stubGlobal("ResizeObserver", original);
    },
  };
}
