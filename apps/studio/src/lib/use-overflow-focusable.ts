import { useEffect, useRef, useState } from "react";

/**
 * For a horizontally scrolling wrapper: `tabIndex` is 0 only while the content overflows, so keyboard
 * users can scroll it, and the wrapper isn't a pointless tab stop otherwise. Follows resizes of the
 * wrapper and of its children (a table growing doesn't resize the wrapper).
 */
export function useOverflowFocusable<T extends HTMLElement = HTMLDivElement>() {
  const ref = useRef<T>(null);
  const [overflows, setOverflows] = useState(false);
  useEffect(() => {
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const check = () => setOverflows(el.scrollWidth > el.clientWidth);
    check();
    const observer = new ResizeObserver(check);
    observer.observe(el);
    for (const child of Array.from(el.children)) observer.observe(child);
    return () => observer.disconnect();
  });
  return { ref, tabIndex: overflows ? (0 as const) : undefined };
}
