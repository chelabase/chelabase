import { expect, test } from '@playwright/test'

for (const width of [1440, 1024, 390]) {
  test(`landing at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 })
    await page.goto('/')
    await page.evaluate(() => document.fonts.ready)
    await expect(page).toHaveScreenshot(`landing-${width}.png`, { fullPage: true })
  })
}
