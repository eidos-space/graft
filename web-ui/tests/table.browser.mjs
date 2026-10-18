// Optional browser regression suite against the real local CLI and disposable fixture.
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import fs from "node:fs";
const browser = await chromium.launch({
  headless: true,
  ...(process.env.CHROMIUM_PATH ? { executablePath: process.env.CHROMIUM_PATH } : {}),
});
const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
const errors = [];
const offsets = [];
page.on("pageerror", (error) => errors.push(error.message));
page.on("request", (request) => {
  if (request.url().endsWith("/api/database")) {
    const input = request.postDataJSON();
    if (input.table === "large" && input.offset !== undefined) offsets.push(input.offset);
  }
});
const output = process.env.SCREENSHOT_DIR ?? "/private/tmp/graft-glide-screenshots";
fs.mkdirSync(output, { recursive: true });
async function selectTable(name) {
  const panel = page.locator('[role="tabpanel"]:visible').last();
  const database = panel.getByRole("treeitem", { name: "data.sqlite", exact: true });
  if ((await database.getAttribute("aria-expanded")) !== "true") await database.click();
  await panel.getByRole("treeitem", { name, exact: true }).click();
  await page.locator(".glide-table canvas").first().waitFor();
}
async function dataContains(text) {
  await page.waitForFunction(
    (text) => document.querySelector('.glide-table [role="grid"]')?.textContent.includes(text),
    text,
  );
}
try {
  await page.goto(process.env.UI_URL);
  await page.getByRole("tab", { name: "All files", exact: true }).click();
  await selectTable("large");
  assert.equal(await page.getByRole("grid").getAttribute("aria-rowcount"), "100001");
  await page
    .locator(".glide-table canvas")
    .first()
    .click({ position: { x: 80, y: 55 } });
  await page.keyboard.press("Control+End");
  await dataContains("Large row 100000");
  assert.ok(offsets.includes(99900), "last page fetched without traversing previous pages");
  assert.ok(offsets.every((offset) => offset <= 100 || offset >= 99800));
  assert.ok((await page.getByRole("row").count()) < 60, "accessibility rows are virtualized too");
  assert.equal(await page.getByRole("button", { name: /Load .*more/ }).count(), 0);
  await page.screenshot({ path: `${output}/large.png` });
  await page.keyboard.press("Control+Home");
  await dataContains("Large row 1");
  await selectTable("wide");
  const scroller = page.locator(".dvn-scroller");
  assert.ok(await scroller.evaluate((node) => node.scrollWidth > node.clientWidth * 3));
  await scroller.evaluate((node) => {
    node.scrollLeft = node.scrollWidth;
  });
  await dataContains("column 47");
  await page.setViewportSize({ width: 800, height: 650 });
  assert.ok((await page.locator(".glide-table canvas").count()) <= 2);
  await page.screenshot({ path: `${output}/wide.png` });
  await page.setViewportSize({ width: 1440, height: 900 });
  await page.getByRole("tab", { name: /^Changes/ }).click();
  await selectTable("notes");
  const filters = page.getByRole("tablist", { name: "Row changes" });
  await filters.getByRole("tab", { name: /^Added/ }).click();
  await dataContains("Inserted value");
  assert.equal(await page.getByRole("grid").getAttribute("aria-rowcount"), "2");
  await filters.getByRole("tab", { name: /^Updates/ }).click();
  await dataContains("Changed 中文内容");
  await page.screenshot({ path: `${output}/updates.png` });
  assert.deepEqual(errors, []);
  console.log(JSON.stringify({ result: "PASS", rows: 100000, offsets, screenshots: output }));
} finally {
  await browser.close();
}
