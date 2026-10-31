/**
 * Regenerates the Studio screenshots on the landing page from a seeded Acme stack.
 *
 *   CHELABASE_STUDIO_URL=http://studio.chelabase.localhost:8000 pnpm --filter @chelabase/landing capture-studio
 *
 * The stack is the Acme example (infra/compose/docker-compose.example.yml): Ada signs in with the
 * seed's demo password. The script only reads, except for one staged schema change that it
 * discards (the page is closed with the change pending, never applied). Images are WebP at 2x,
 * rendered by Chromium itself, so nothing besides Playwright is needed.
 */
import { mkdir, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { chromium, type Page } from "@playwright/test";

const studio = (process.env.CHELABASE_STUDIO_URL ?? "").replace(/\/+$/, "");
if (studio === "") {
  console.error("Set CHELABASE_STUDIO_URL to the seeded stack's Studio URL, e.g. http://studio.chelabase.localhost:8000");
  process.exit(1);
}
const email = process.env.CHELABASE_DEMO_EMAIL ?? "ada@acme.example";
const password = process.env.CHELABASE_DEMO_PASSWORD ?? "Acme-Demo-2026!";
const outDir = fileURLToPath(new URL("../public/screens/", import.meta.url));

/** One fixed viewport for every shot; the saved image is this at 2x (or the clip of it). */
const VIEWPORT = { width: 1152, height: 800 };
/** The branches list is taller than the viewport, so that shot is taken in a taller window. */
const TALL = { width: VIEWPORT.width, height: 1100 };
const SCALE = 2;
const WEBP_QUALITY = 0.8;

async function signIn(page: Page): Promise<void> {
  await page.goto(`${studio}/`);
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.getByRole("textbox", { name: "Email" }).fill(email);
  await page.getByRole("textbox", { name: "Password" }).fill(password);
  await page.getByRole("button", { name: "Sign in" }).click();
  await page.waitForURL(/\/app(\/|$)/, { timeout: 60_000 });
  await page.getByRole("link", { name: "Acme", exact: true }).waitFor();
}

/** Chromium re-encodes the PNG as WebP, so the script needs no encoder of its own. */
async function toWebp(page: Page, png: Buffer): Promise<Buffer> {
  const b64 = await page.evaluate(
    async ([src, quality]) => {
      const img = new Image();
      img.src = `data:image/png;base64,${src}`;
      await img.decode();
      const canvas = document.createElement("canvas");
      canvas.width = img.naturalWidth;
      canvas.height = img.naturalHeight;
      canvas.getContext("2d")!.drawImage(img, 0, 0);
      return canvas.toDataURL("image/webp", quality as number).split(",")[1]!;
    },
    [png.toString("base64"), WEBP_QUALITY] as const,
  );
  return Buffer.from(b64, "base64");
}

async function save(encoder: Page, name: string, png: Buffer): Promise<void> {
  const webp = await toWebp(encoder, png);
  await writeFile(`${outDir}${name}.webp`, webp);
  console.log(`${name}.webp  ${(webp.length / 1024).toFixed(0)} KB`);
}

/** The acme database's table editor URL, from the project page's Tables link. */
async function tablesUrl(page: Page): Promise<string> {
  await page.getByRole("link", { name: "Acme", exact: true }).click();
  const link = page.getByRole("link", { name: "Tables" }).last();
  await link.waitFor();
  const href = await link.getAttribute("href");
  if (href === null) throw new Error("no Tables link on the Acme project page");
  return href;
}

async function main(): Promise<void> {
  await mkdir(outDir, { recursive: true });
  const browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: VIEWPORT,
    deviceScaleFactor: SCALE,
    colorScheme: "dark",
    reducedMotion: "reduce",
  });
  const page = await context.newPage();
  // A staged change makes the page ask before it unloads; close without applying it.
  page.on("dialog", (d) => void d.accept());
  const encoder = await context.newPage();
  await encoder.goto("about:blank");

  await signIn(page);
  const tables = await tablesUrl(page);
  const projectUrl = page.url();

  // Manage: sales.orders with an added column staged in the review panel.
  await page.goto(`${studio}${tables}`);
  await page.getByText("Pick a table").waitFor();
  const here = new URL(page.url());
  await page.goto(`${studio}${here.pathname.replace(/\/[^/]+$/, "")}/sales/orders`);
  await page.getByRole("tab", { name: "Structure" }).or(page.getByText("Structure", { exact: true })).first().click();
  await page.getByText("Constraints", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Add column" }).locator("visible=true").first().click();
  await page.getByRole("textbox", { name: "Name" }).fill("shipped_at");
  await page.getByRole("combobox", { name: "Type" }).click();
  await page.getByRole("option", { name: "timestamptz", exact: true }).click();
  await page.getByRole("button", { name: "Add to changes" }).click();
  await page.getByText(/Dry run passed/).waitFor();
  await save(encoder, "manage-table-editor", await page.screenshot());

  // Manage: the RLS warning on the Data API panel, cropped to the warning.
  await page.goto(projectUrl);
  await page.getByRole("button", { name: "Data API" }).click();
  const warning = page.getByRole("region", { name: "RLS warning" });
  await warning.getByText("public.employees").waitFor();
  await save(encoder, "manage-rls-warning", await warning.screenshot());

  // Collaborate: the Finance team's project access (Developer on Acme, acting as finance). On the
  // team page it sits below the member list, so the shot crops the access section.
  await page.goto(`${studio}/app/members?tab=teams`);
  await page.getByRole("row", { name: /Finance/ }).getByText("Finance").click();
  const access = page.locator("section").filter({ has: page.getByText("Project access", { exact: true }) }).last();
  await access.getByText("Developer · finance").waitFor();
  await access.scrollIntoViewIfNeeded();
  await save(encoder, "collaborate-team-access", await access.screenshot());

  // Iterate: the branches list, with protected main and feature-discounts.
  await page.setViewportSize(TALL);
  await page.goto(projectUrl);
  await page.getByRole("button", { name: "Branches" }).click();
  await page.getByText("feature-discounts").first().waitFor();
  await page.getByText("main can't be deleted; delete the database instead").waitFor();
  const row = (await page.getByRole("row", { name: /^feature-discounts/ }).first().boundingBox())!;
  await save(
    encoder,
    "iterate-branches",
    await page.screenshot({ clip: { x: 0, y: 0, width: VIEWPORT.width, height: Math.ceil(row.y + row.height + 16) } }),
  );

  await browser.close();
}

await main();
