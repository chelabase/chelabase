import defaultMdxComponents from 'fumadocs-ui/mdx';
import { Callout } from 'fumadocs-ui/components/callout';
import * as Twoslash from 'fumadocs-twoslash/ui';
import type { MDXComponents } from 'mdx/types';

const ISSUES_URL = 'https://github.com/chelabase/chelabase/issues';

/** The alpha notice, word for word as the README and the landing page carry it; one place to change it in the docs. */
function AlphaNotice() {
  return (
    <Callout type="warn" title="Alpha">
      Chelabase is early, pre-release software and not ready for production data. Expect breaking changes between
      versions, rough edges and security issues no one has found yet. Run it on data you can afford to lose, and{' '}
      <a href={ISSUES_URL}>report what breaks</a>.
    </Callout>
  );
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    ...Twoslash,
    AlphaNotice,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}
