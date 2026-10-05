// Server-only: `fumadocs-openapi/server` imports node:fs and node:path. Import this module only
// from createServerFn handlers and from the llms and search routes, never from client code.
import { existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { llms, loader } from 'fumadocs-core/source';
import { lucideIconsPlugin } from 'fumadocs-core/source/lucide-icons';
import { createOpenAPI } from 'fumadocs-openapi/server';
import { docs } from './source';
import { docsRoute } from './shared';

// The repository root, found by walking up to the first directory that holds openapi/. This
// module runs from src/lib in dev and from the bundled dist/server in the build, so a fixed
// relative path would be wrong in one of them; input paths resolve against process.cwd() otherwise.
function findRoot(from: string): string {
  for (let dir = from; ; dir = dirname(dir)) {
    if (existsSync(resolve(dir, 'openapi/management.json'))) return dir;
    if (dirname(dir) === dir) throw new Error(`openapi/management.json not found above ${from}`);
  }
}

const root = findRoot(import.meta.dirname);

export const apis = {
  management: createOpenAPI({ input: [resolve(root, 'openapi/management.json')] }),
  login: createOpenAPI({ input: [resolve(root, 'openapi/login.json')] }),
  data: createOpenAPI({ input: [resolve(root, 'openapi/data.json')] }),
} as const;

async function build() {
  return loader(
    {
      docs: docs.toFumadocsSource(),
      management: await apis.management.staticSource({
        baseDir: 'reference/management',
        per: 'operation',
        groupBy: 'tag',
        meta: true,
      }),
      login: await apis.login.staticSource({
        baseDir: 'architecture/login-api',
        per: 'operation',
        groupBy: 'tag',
        meta: true,
      }),
      data: await apis.data.staticSource({
        baseDir: 'reference/data',
        per: 'operation',
        // Two operations under one tag: grouping would repeat the folder (reference/data/data).
        groupBy: 'none',
        meta: true,
      }),
    },
    {
      baseUrl: docsRoute,
      plugins: [
        lucideIconsPlugin(),
        apis.management.loaderPlugin(),
        apis.login.loaderPlugin(),
        apis.data.loaderPlugin(),
      ],
    },
  );
}

export type Source = Awaited<ReturnType<typeof build>>;

let memo: Promise<Source> | undefined;

export function getSource(): Promise<Source> {
  memo ??= build();
  return memo;
}

/** True for the virtual pages `staticSource` makes from an OpenAPI document. */
export function isApiPage(page: { data: object }): page is {
  data: { getOpenAPIPageProps: () => Record<string, unknown> };
} {
  return 'getOpenAPIPageProps' in page.data;
}

export async function getLlms() {
  const source = await getSource();
  return llms(source, {
    renderPage: async (page) => {
      const head = `# ${page.data.title} (${page.url})`;
      if (isApiPage(page)) return `${head}\n\n${page.data.description ?? ''}`;
      return `${head}\n\n${await (page.data as { getText(kind: 'processed'): Promise<string> }).getText('processed')}`;
    },
  });
}
