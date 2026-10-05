import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
import { findBrokenLinks } from './links';

describe('findBrokenLinks', () => {
  it('reports_a_missing_page_and_a_missing_fragment', () => {
    const broken = findBrokenLinks(resolve(import.meta.dirname, 'fixtures/dist-broken'));
    expect(broken).toEqual([
      { page: 'docs/a/index.html', href: '/docs/a#nope', reason: 'missing-fragment' },
      { page: 'index.html', href: '/docs/missing', reason: 'missing-page' },
    ]);
  });
});
