import { vi } from "vitest";

type Matcher = (query: string) => boolean;

/**
 * Replaces `window.matchMedia` with a switchable mock. Every call reads the current state, so
 * `useMediaQuery` (which calls it on each render) follows `set`; `set` notifies every "change"
 * listener registered on any returned list. Wrap `set` in `act`. `restore` puts the original back.
 */
export function mockMatchMedia(initial: Matcher): { set(matches: Matcher): void; restore(): void } {
  let current = initial;
  const listeners = new Set<(e: MediaQueryListEvent) => void>();
  const had = Object.prototype.hasOwnProperty.call(window, "matchMedia");
  const original = window.matchMedia;
  const impl = (query: string) => ({
    media: query,
    onchange: null,
    get matches() {
      return current(query);
    },
    addEventListener: (type: string, cb: (e: MediaQueryListEvent) => void) => {
      if (type === "change") listeners.add(cb);
    },
    removeEventListener: (type: string, cb: (e: MediaQueryListEvent) => void) => {
      if (type === "change") listeners.delete(cb);
    },
    addListener: (cb: (e: MediaQueryListEvent) => void) => listeners.add(cb),
    removeListener: (cb: (e: MediaQueryListEvent) => void) => listeners.delete(cb),
    dispatchEvent: () => false,
  });
  Object.defineProperty(window, "matchMedia", { value: vi.fn(impl), writable: true, configurable: true });
  return {
    set(matches) {
      current = matches;
      for (const cb of [...listeners]) cb({ matches: matches(""), media: "" } as MediaQueryListEvent);
    },
    restore() {
      if (had) Object.defineProperty(window, "matchMedia", { value: original, writable: true, configurable: true });
      else delete (window as unknown as Record<string, unknown>).matchMedia;
    },
  };
}
