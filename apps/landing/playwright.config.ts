import { defineConfig } from '@playwright/test'

export default defineConfig({
  testDir: 'visual',
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
  use: {
    baseURL: 'http://localhost:4173',
    reducedMotion: 'reduce',
    colorScheme: 'dark',
    deviceScaleFactor: 1,
  },
  expect: {
    toHaveScreenshot: { maxDiffPixelRatio: 0.002, animations: 'disabled' },
  },
  webServer: {
    command: 'node_modules/.bin/vite preview --port 4173 --strictPort',
    port: 4173,
    reuseExistingServer: false,
  },
})
