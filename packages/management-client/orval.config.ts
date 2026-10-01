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
      override: {
        mutator: { path: "src/fetch.ts", name: "customFetch" },
        fetch: { includeHttpResponseReturnType: false },
      },
    },
  },
});
