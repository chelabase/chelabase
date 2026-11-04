import { describe, expect, it } from 'vitest';
import { docsHref, GITHUB_URL } from './site-urls';

describe('docsHref', () => {
  it('docs_links_use_the_docs_site_when_set', () => {
    expect(docsHref('using/quickstart', 'https://docs.test')).toBe('https://docs.test/using/quickstart');
    expect(docsHref('using/quickstart', 'https://docs.test//')).toBe('https://docs.test/using/quickstart');
    expect(docsHref('using/quickstart', ' https://docs.test/ ')).toBe('https://docs.test/using/quickstart');
  });

  it('docs_links_fall_back_to_the_mdx_on_github', () => {
    const mdx = `${GITHUB_URL}/blob/main/apps/docs/content/docs/architecture/gateway.mdx`;
    expect(docsHref('architecture/gateway')).toBe(mdx);
    expect(docsHref('architecture/gateway', '')).toBe(mdx);
    expect(docsHref('architecture/gateway', '   ')).toBe(mdx);
  });
});
