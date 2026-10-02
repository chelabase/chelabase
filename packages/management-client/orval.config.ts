import { defineConfig } from "orval";

export default defineConfig({
  management: {
    input: "../../openapi/management.json",
    output: {
      client: "react-query",
      httpClient: "fetch",
      mode: "split",
      target: "src/generated/endpoints.ts",
      schemas: "src/generated/model",
      clean: true,
      // Path parameters are one encoded segment each: a role made in SQL may be named
      // "%61pp" or "a/b", which must not reach another role or route.
      urlEncodeParameters: true,
      override: {
        mutator: { path: "src/fetch.ts", name: "customFetch" },
        fetch: { includeHttpResponseReturnType: false },
      },
    },
  },
});
