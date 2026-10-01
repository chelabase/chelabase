import { defineConfig } from "orval";

export default defineConfig({
  login: {
    input: "../../../../../openapi/login.json",
    output: {
      client: "react-query",
      httpClient: "fetch",
      mode: "split",
      target: "generated/endpoints.ts",
      schemas: "generated/model",
      clean: true,
      override: {
        mutator: { path: "fetch.ts", name: "customFetch" },
        fetch: { includeHttpResponseReturnType: false },
      },
    },
  },
});
