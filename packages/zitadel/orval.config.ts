import { defineConfig } from "orval";

export default defineConfig({
  zitadel: {
    input: "./openapi/zitadel-v2.71.19.openapi.yaml",
    output: {
      target: "./src/generated/zitadel.ts",
      client: "fetch",
      mode: "single",
      clean: true,
      override: {
        mutator: {
          path: "./src/mutator.ts",
          name: "customFetch",
        },
      },
    },
  },
});
