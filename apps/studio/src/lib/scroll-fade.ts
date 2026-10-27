export interface ScrollFade {
  start: boolean;
  end: boolean;
}

/** Which edges of a horizontal scroller have more content past them (1px of slack for rounding). */
export function scrollFade(scrollLeft: number, clientWidth: number, scrollWidth: number): ScrollFade {
  if (scrollWidth - clientWidth <= 1) return { start: false, end: false };
  return { start: scrollLeft > 1, end: scrollLeft + clientWidth < scrollWidth - 1 };
}

/** A CSS mask fading the edges that have more content. */
export function fadeMask({ start, end }: ScrollFade): string | undefined {
  if (!start && !end) return undefined;
  return `linear-gradient(to right, ${start ? "transparent, #000 1.5rem" : "#000, #000"}, ${end ? "#000 calc(100% - 1.5rem), transparent" : "#000, #000"})`;
}
