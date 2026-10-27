export interface Measured {
  scroll: number;
  client: number;
  /** The last crumb: it may be cut down to `MIN_LAST_CRUMB` before the trail counts as not fitting. */
  last?: boolean;
}

/** About 8rem: the least a truncated last crumb still shows. */
export const MIN_LAST_CRUMB = 128;

/**
 * The width a trail needs: the width it has, plus what each truncated item (scroll wider than
 * client) is missing. The last crumb counts at `minLast` at most, and gives back what it has beyond it (a long name is
 * cut, not a reason to collapse the middle). Under 1px of difference is rounding, not truncation.
 */
export function neededTrailWidth(navWidth: number, items: readonly Measured[], minLast = MIN_LAST_CRUMB): number {
  let missing = 0;
  for (const { scroll, client, last } of items) {
    // The last crumb can be cut down to `minLast`, so a wider one gives that slack back.
    if (last) missing += Math.min(scroll, minLast) - client;
    else if (scroll - client > 1) missing += scroll - client;
  }
  return navWidth + missing;
}
