import { useSyncExternalStore } from "react";

/** Whether a media query matches now, following changes. False where `matchMedia` is missing (tests, SSR). */
export function useMediaQuery(query: string): boolean {
  return useSyncExternalStore(
    (onChange) => {
      if (typeof window === "undefined" || !window.matchMedia) return () => {};
      const list = window.matchMedia(query);
      list.addEventListener("change", onChange);
      return () => list.removeEventListener("change", onChange);
    },
    () => (typeof window !== "undefined" && window.matchMedia ? window.matchMedia(query).matches : false),
    () => false,
  );
}

/** The phone-width breakpoint shared by the rail, the drawer and the breadcrumbs. */
export const NARROW_QUERY = "(width < 40rem)";
