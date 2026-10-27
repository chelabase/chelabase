import { useCallback, useEffect, useRef, useState } from "react";
import { fadeMask, scrollFade, type ScrollFade } from "./scroll-fade";

/** A horizontal scroller's ref, scroll handler and mask style: the edges with more content behind them fade. */
export function useScrollFade<T extends HTMLElement>() {
  const ref = useRef<T>(null);
  const [fade, setFade] = useState<ScrollFade>({ start: false, end: false });
  const measure = useCallback(() => {
    const el = ref.current;
    if (!el) return;
    const next = scrollFade(el.scrollLeft, el.clientWidth, el.scrollWidth);
    setFade((prev) => (prev.start === next.start && prev.end === next.end ? prev : next));
  }, []);
  useEffect(() => {
    measure();
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [measure]);
  const mask = fadeMask(fade);
  return { ref, onScroll: measure, style: { maskImage: mask, WebkitMaskImage: mask } };
}
