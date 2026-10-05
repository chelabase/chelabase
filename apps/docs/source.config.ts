import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { defineConfig } from 'fumadocs-mdx/config';
import { rehypeCodeDefaultOptions } from 'fumadocs-core/mdx-plugins/rehype-code';
import { transformerTwoslash } from 'fumadocs-twoslash';

const here = import.meta.dirname;

// The golden generated types from the client's fixtures, available to every sample as './db':
//   import { type Database, types } from './db';
const dbTypes = readFileSync(resolve(here, '../../packages/client/fixtures/gen/db.ts'), 'utf8');

export default defineConfig({
  mdxOptions: {
    rehypeCodeOptions: {
      ...rehypeCodeDefaultOptions,
      transformers: [
        ...(rehypeCodeDefaultOptions.transformers ?? []),
        transformerTwoslash({
          twoslashOptions: {
            // Twoslash does not read tsconfig.json; it resolves '@chelabase/client' by walking up
            // from here, through apps/docs/node_modules.
            cwd: here,
            extraFiles: { 'db.ts': dbTypes },
          },
        }),
      ],
    },
  },
});
