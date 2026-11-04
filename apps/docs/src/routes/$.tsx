import { createFileRoute, Link, notFound } from '@tanstack/react-router';
import { DocsLayout } from 'fumadocs-ui/layouts/docs';
import { createServerFn } from '@tanstack/react-start';
import { docs } from '@/lib/source';
import {
  DocsBody,
  DocsDescription,
  DocsPage,
  DocsTitle,
  MarkdownCopyButton,
} from 'fumadocs-ui/layouts/docs/page';
import { baseOptions } from '@/lib/layout.shared';
import { appName, canonicalUrl, getPageMarkdownUrl, socialMeta } from '@/lib/shared';
import { staticFunctionMiddleware } from '@tanstack/start-static-server-functions';
import { useFumadocsLoader } from 'fumadocs-core/source/client';
import type { TOCItemType } from 'fumadocs-core/toc';
import { Suspense, use } from 'react';
import { useMDXComponents } from '@/components/mdx';
import { OpenAPIPage } from '@/components/api-page';
import type { ComponentProps } from 'react';

type Json = string | number | boolean | null | Json[] | { [key: string]: Json };

export const Route = createFileRoute('/$')({
  component: Page,
  loader: async ({ params }) => {
    const slugs = params._splat?.split('/') ?? [];
    const data = await loader({ data: slugs });
    if (data.kind === 'doc') await docs.getPage(data.path)?.preload();
    return data;
  },
  // The root route's title ("Chelabase") is the fallback; a page's own title replaces it.
  head: ({ loaderData, params }) => {
    if (!loaderData) return {};
    const title = `${loaderData.title} · ${appName}`;
    const description = loaderData.description ? plainText(loaderData.description) : undefined;
    const url = canonicalUrl(`/${params._splat ?? ''}`);
    return {
      meta: [
        { title },
        ...(description ? [{ name: 'description', content: description }] : []),
        ...socialMeta(title, description, url),
      ],
      links: [{ rel: 'canonical', href: url }],
    };
  },
});

/** A description as plain text for <meta>: an API operation's is markdown, and its code spans keep their backticks. */
function plainText(markdown: string): string {
  return markdown.replace(/`([^`]*)`/g, '$1');
}

// The only place the server-only source (and so fumadocs-openapi/server) is reached from a route:
// the handler is compiled out of the client bundle, and `staticFunctionMiddleware` caches its
// output as JSON at build time.
const loader = createServerFn({
  method: 'GET',
})
  .validator((slugs: string[]) => slugs)
  .middleware([staticFunctionMiddleware])
  .handler(async ({ data: slugs }) => {
    const { getSource, isApiPage } = await import('@/lib/api-source.server');
    const source = await getSource();
    const page = source.getPage(slugs);
    if (!page) throw notFound();
    const pageTree = await source.serializePageTree(source.getPageTree());

    if (isApiPage(page)) {
      return {
        kind: 'api' as const,
        pageTree,
        title: page.data.title as string,
        description: page.data.description as string | undefined,
        // Plain strings: the TOC's ReactNode titles are not serializable.
        toc: (page.data as { toc: TOCItemType[] }).toc.map((item) => ({
          title: String(item.title ?? ''),
          url: item.url,
          depth: item.depth,
        })),
        props: page.data.getOpenAPIPageProps() as unknown as Json,
      };
    }

    return {
      kind: 'doc' as const,
      pageTree,
      path: page.path,
      title: page.data.title as string,
      description: page.data.description as string | undefined,
      markdownUrl: getPageMarkdownUrl(page).url,
    };
  });

function DocContent({ path, markdownUrl }: { path: string; markdownUrl: string }) {
  const page = docs.getPage(path);
  if (!page) throw new Error(`unknown page: ${path}`);

  const { toc } = use(page.load());
  const MDX = page.body;

  return (
    <DocsPage toc={toc}>
      <DocsTitle>{page.title}</DocsTitle>
      <DocsDescription>{page.description}</DocsDescription>
      <div className="flex flex-row gap-2 items-center border-b -mt-4 pb-6">
        <MarkdownCopyButton markdownUrl={markdownUrl} />
      </div>
      <DocsBody>
        <MDX components={useMDXComponents()} />
      </DocsBody>
    </DocsPage>
  );
}

function Page() {
  const data = useFumadocsLoader(Route.useLoaderData());

  return (
    <DocsLayout {...baseOptions()} tree={data.pageTree}>
      {data.kind === 'doc' ? (
        <>
          <Link to={data.markdownUrl} hidden />
          <Suspense>
            <DocContent path={data.path} markdownUrl={data.markdownUrl} />
          </Suspense>
        </>
      ) : (
        <DocsPage toc={data.toc}>
          <DocsTitle>{data.title}</DocsTitle>
          {/* No DocsDescription: the description is markdown, and OpenAPIPage renders it itself. */}
          <DocsBody>
            <OpenAPIPage {...(data.props as unknown as ComponentProps<typeof OpenAPIPage>)} />
          </DocsBody>
        </DocsPage>
      )}
    </DocsLayout>
  );
}
