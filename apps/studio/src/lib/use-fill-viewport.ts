import { useLayoutEffect, useRef, useState } from "react";

/**
 * The height that fills the viewport below the element's top (the shell header's height isn't
 * hard-coded here): measured on mount and on resize.
 */
export function useFillViewport() {
  const ref = useRef<HTMLDivElement>(null);
  const [top, setTop] = useState<number | null>(null);
  useLayoutEffect(() => {
    const measure = () => {
      const el = ref.current;
      if (el) setTop(Math.max(0, el.getBoundingClientRect().top + window.scrollY));
    };
    measure();
    window.addEventListener("resize", measure);
    return () => window.removeEventListener("resize", measure);
  }, []);
  return { ref, style: top === null ? undefined : { height: `calc(100dvh - ${top}px)` } };
}
