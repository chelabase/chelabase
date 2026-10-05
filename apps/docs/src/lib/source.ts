import { defineDocs } from 'fumadocs-mdx/macro';

// Client-safe: only the MDX collection (page bodies are loaded in the browser through it).
// The page tree, the OpenAPI pages and the llms output are built in api-source.server.ts.
export const docs = defineDocs({
  dir: 'content/docs',
  docs: {
    async: true,
    postprocess: {
      includeProcessedMarkdown: true,
    },
  },
});
