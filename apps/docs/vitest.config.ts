import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'node',
    include: ['tests/**/*.test.ts'],
    // The build tests walk and read hundreds of prerendered files; the link check alone can
    // outlast vitest's 5 s default on a cold disk cache.
    testTimeout: 30_000,
  },
});
