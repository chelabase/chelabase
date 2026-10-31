import { expect, test } from '@playwright/test'

for (const width of [1440, 1024, 390]) {
  test(`landing at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 })
    await page.goto('/')
    await page.evaluate(() => document.fonts.ready)
    // Full-page capture doesn't scroll, so lazy images would stay blank: load them all first.
    await page.evaluate(async () => {
      const imgs = [...document.images];
      for (const img of imgs) img.loading = "eager";
      await Promise.all(imgs.map((img) => img.decode()));
    })
    await expect(page).toHaveScreenshot(`landing-${width}.png`, { fullPage: true })
  })
}
