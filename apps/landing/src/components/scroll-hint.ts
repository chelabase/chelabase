import { useEffect, useState, type RefObject } from "react";

export interface ScrollHint {
  /** The content is wider than the box, so it scrolls sideways. */
  scrollable: boolean;
  /** Content is still hidden past the right edge. */
  more: boolean;
}

/** Where a sideways-scrolling box stands. A pixel of slack absorbs fractional widths. */
export function scrollHint(box: { scrollLeft: number; clientWidth: number; scrollWidth: number }): ScrollHint {
  const hidden = box.scrollWidth - box.clientWidth;
  return { scrollable: hidden > 1, more: hidden - box.scrollLeft > 1 };
}

/** Tracks `scrollHint` for a box as it scrolls or resizes. Nothing scrolls before hydration. */
export function useScrollHint(ref: RefObject<HTMLElement | null>): ScrollHint {
  const [hint, setHint] = useState<ScrollHint>({ scrollable: false, more: false });
  useEffect(() => {
    const box = ref.current;
    if (!box) return;
    const update = () =>
      setHint((prev) => {
        const next = scrollHint(box);
        return next.scrollable === prev.scrollable && next.more === prev.more ? prev : next;
      });
    update();
    box.addEventListener("scroll", update, { passive: true });
    // Where there is no ResizeObserver (older browsers, jsdom), the window's resize stands in.
    const resize = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(update);
    if (resize) resize.observe(box);
    else window.addEventListener("resize", update);
    return () => {
      box.removeEventListener("scroll", update);
      if (resize) resize.disconnect();
      else window.removeEventListener("resize", update);
    };
  }, [ref]);
  return hint;
}
