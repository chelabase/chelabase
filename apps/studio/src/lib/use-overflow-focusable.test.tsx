import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useOverflowFocusable } from "./use-overflow-focusable";

let callbacks: Array<() => void> = [];
let observed: Element[] = [];
let disconnected = 0;

beforeEach(() => {
  callbacks = [];
  observed = [];
  disconnected = 0;
  vi.stubGlobal(
    "ResizeObserver",
    class {
      constructor(cb: () => void) {
        callbacks.push(cb);
      }
      observe(el: Element) {
        observed.push(el);
      }
      disconnect() {
        disconnected++;
      }
      unobserve() {}
    },
  );
});
afterEach(() => vi.unstubAllGlobals());

function Scroller() {
  const { ref, tabIndex } = useOverflowFocusable();
  return (
    <div ref={ref} tabIndex={tabIndex} role="region" aria-label="Wide" data-testid="box">
      <table />
    </div>
  );
}

function size(el: HTMLElement, scrollWidth: number, clientWidth: number) {
  Object.defineProperty(el, "scrollWidth", { configurable: true, value: scrollWidth });
  Object.defineProperty(el, "clientWidth", { configurable: true, value: clientWidth });
}

describe("useOverflowFocusable", () => {
  it("is not focusable when the content fits", () => {
    render(<Scroller />);
    expect(screen.getByTestId("box")).not.toHaveAttribute("tabindex");
  });

  it("becomes focusable when the content overflows, and drops it again when it fits", () => {
    render(<Scroller />);
    const box = screen.getByTestId("box");
    size(box, 900, 300);
    act(() => callbacks.at(-1)!());
    expect(box).toHaveAttribute("tabindex", "0");
    size(box, 300, 300);
    act(() => callbacks.at(-1)!());
    expect(box).not.toHaveAttribute("tabindex");
  });

  it("observes the wrapper and its children, and disconnects on unmount", () => {
    const { unmount } = render(<Scroller />);
    const box = screen.getByTestId("box");
    expect(observed).toContain(box);
    expect(observed).toContain(box.firstElementChild);
    unmount();
    expect(disconnected).toBeGreaterThan(0);
  });

  it("does nothing without ResizeObserver", () => {
    vi.stubGlobal("ResizeObserver", undefined);
    render(<Scroller />);
    expect(screen.getByTestId("box")).not.toHaveAttribute("tabindex");
  });
});
