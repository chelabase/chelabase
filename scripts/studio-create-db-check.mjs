// Usage: node scripts/studio-create-db-check.mjs <studio base url> <email> <password> [persistent]
//   e.g. node scripts/studio-create-db-check.mjs http://studio.<slug>.chelabase.localhost:8100 \
//          dev@chelabase.localhost "$(jq -r .password <stack dir>/bootstrap/dev-member.json)"
//
// Studio "Create database" input check, through the real sign-in form, in headless Chromium.
// Exit 0 (prints OK): typing enables the button and the click sends the create request.
// The create POST is intercepted and aborted, so the stack's state isn't changed.
//
// Default mode: an isolated (in-memory) browser context, as the project's Playwright MCP runs.
// `persistent`: the documented negative case, a browser with an on-disk profile. After a form
// sign-in, Chrome opens its "Save password?" prompt, invisible headless, which swallows all
// input to the tab: this mode is expected to FAIL ("input dropped").
//
// Needs the demo data: a project named "default" in the signed-in account.
// Playwright comes from the workspace (apps/landing's `@playwright/test`); run `pnpm install`
// first, and `pnpm --filter landing exec playwright install chromium` if the browser is missing.
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const require = createRequire(new URL("../apps/landing/package.json", import.meta.url));
const { chromium } = require("@playwright/test");

const [base, email, password, mode] = process.argv.slice(2);
if (!base || !email || !password || (mode !== undefined && mode !== "persistent")) {
  console.error("usage: node scripts/studio-create-db-check.mjs <studio base url> <email> <password> [persistent]");
  process.exit(2);
}

let context;
let browser;
let profileDir;
if (mode === "persistent") {
  // What the Playwright MCP server does without `--isolated`: a full Chrome with an on-disk profile.
  profileDir = fs.mkdtempSync(path.join(os.tmpdir(), "studio-check-"));
  context = await chromium.launchPersistentContext(profileDir, {
    channel: "chromium",
    headless: true,
  });
} else {
  browser = await chromium.launch({ channel: "chromium", headless: true });
  context = await browser.newContext();
}
// Closes the browser and removes the temporary profile; every exit path goes through it.
const finish = async (code) => {
  await context.close().catch(() => {});
  await browser?.close().catch(() => {});
  if (profileDir) fs.rmSync(profileDir, { recursive: true, force: true });
  process.exit(code);
};
const fail = async (msg) => {
  console.error(`FAIL: ${msg}`);
  await finish(1);
};
try {
  const page = context.pages()[0] ?? (await context.newPage());
  let createRequest = null;
  await context.route(/\/databases(\?|$)/, (route) => {
    if (route.request().method() === "POST") {
      createRequest = route.request().url();
      return route.abort();
    }
    return route.continue();
  });
  await page.goto(base + "/");
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.waitForURL(/\/login\?/);
  await page.getByRole("textbox", { name: "Email" }).fill(email);
  await page.getByRole("textbox", { name: "Password" }).fill(password);
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.waitForURL(/\/app$/);
  await page.getByRole("link", { name: "default" }).click({ timeout: 10000 });
  await page
    .waitForURL(/\/app\/projects\//, { timeout: 10000 })
    .catch(() => fail("clicking the project link did not navigate (input dropped)"));
  const field = page.getByRole("textbox", { name: "Database name" });
  const button = page.getByRole("button", { name: "Create database" });
  await field.pressSequentially("regress1", { timeout: 10000 });
  if ((await field.inputValue()) !== "regress1") await fail(`typed text did not land: "${await field.inputValue()}"`);
  if (!(await button.isEnabled())) await fail("Create database stayed disabled after typing");
  await button.click({ timeout: 10000 });
  for (let i = 0; i < 50 && !createRequest; i++) await page.waitForTimeout(100);
  if (!createRequest) await fail("clicking Create database sent no request");
  console.log(`OK: create request sent to ${createRequest} (aborted)`);
  await finish(0);
} catch (e) {
  await fail(String(e).split("\n")[0]);
}
