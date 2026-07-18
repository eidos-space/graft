import { expect, test, type Page } from "@playwright/test";

interface CommandResult {
  code: number;
  stderr: string[];
  stdout: string[];
}

async function run(page: Page, args: string[]) {
  const result = await page.evaluate(async (command) => {
    const client = (
      window as typeof window & {
        graftTestClient: {
          run(args: string[]): Promise<CommandResult>;
        };
      }
    ).graftTestClient;
    return client.run(command);
  }, args);
  expect(result.code, result.stderr.join("\n")).toBe(0);
  return result;
}

async function writeOpfs(page: Page, path: string, contents: string) {
  await page.evaluate(
    async ({ candidate, value }) => {
      const parts = candidate.split("/").filter(Boolean);
      let directory = await navigator.storage.getDirectory();
      for (const part of parts.slice(0, -1)) {
        directory = await directory.getDirectoryHandle(part, { create: true });
      }
      const handle = await directory.getFileHandle(parts.at(-1)!, { create: true });
      const writable = await handle.createWritable();
      await writable.write(value);
      await writable.close();
    },
    { candidate: path, value: contents },
  );
}

async function copyAssetToOpfs(page: Page, path: string, source: string) {
  await page.evaluate(
    async ({ candidate, url }) => {
      const parts = candidate.split("/").filter(Boolean);
      let directory = await navigator.storage.getDirectory();
      for (const part of parts.slice(0, -1)) {
        directory = await directory.getDirectoryHandle(part, { create: true });
      }
      const handle = await directory.getFileHandle(parts.at(-1)!, { create: true });
      const bytes = await (await fetch(url)).arrayBuffer();
      const writable = await handle.createWritable();
      await writable.write(bytes);
      await writable.close();
    },
    { candidate: path, url: source },
  );
}

async function seedRepository(page: Page) {
  await page.goto("/e2e.html?reset=1");
  await expect(page.locator("html")).toHaveAttribute("data-ready", "true");
  await run(page, ["init"]);
  await writeOpfs(page, "README.md", "# First revision\n");
  await writeOpfs(page, "notes.txt", "initial notes\n");
  await copyAssetToOpfs(page, "assets/sample.png", "/demo-assets/graft-app-state.png");
  await run(page, ["add", "--all"]);
  await run(page, ["commit", "-m", "Seed worktree"]);

  await writeOpfs(page, "README.md", "# Staged revision\n");
  await run(page, ["add", "README.md"]);
  await writeOpfs(page, "README.md", "# Staged and unstaged revision\n");
  await writeOpfs(page, "draft.txt", "not staged\n");
}

async function openVersionPanel(page: Page) {
  await page.goto("/");
  await page.locator(".sidebar-tabs button").nth(1).click();
  await expect(page.locator(".version-panel")).toBeVisible({ timeout: 30_000 });
}

test("history uses vertical files above an editor-aligned terminal and changes remain split", async ({
  page,
}) => {
  await page.addInitScript(() => {
    localStorage.setItem("graft-guide-open", "false");
    localStorage.setItem("graft-language", "en");
  });
  await seedRepository(page);
  await openVersionPanel(page);
  const sections = page.locator(".changes-section");
  await expect(sections).toHaveCount(2);
  await expect(sections.nth(0)).toContainText("STAGED CHANGES");
  await expect(sections.nth(1)).toContainText("CHANGES");
  await expect(sections.nth(0).locator(".change-row")).toHaveCount(1);
  await expect(sections.nth(1).locator(".change-row")).toHaveCount(2);
  await page.screenshot({ path: "test-results/staged-and-unstaged-sections.png" });

  await page.locator(".segmented-control button").nth(1).click();
  await page.locator(".history-entry > button").first().click();
  const files = page.locator(".commit-file-list > button");
  await expect(files).toHaveCount(3);
  const first = await files.nth(0).boundingBox();
  const second = await files.nth(1).boundingBox();
  expect(first).not.toBeNull();
  expect(second).not.toBeNull();
  expect(Math.abs(first!.x - second!.x)).toBeLessThan(1);
  expect(second!.y).toBeGreaterThanOrEqual(first!.y + first!.height - 1);

  const sidebar = await page.locator(".ide-sidebar").boundingBox();
  const editor = await page.locator(".primary-surface").boundingBox();
  const content = await page.locator(".primary-content").boundingBox();
  const terminal = await page.locator(".terminal-dock").boundingBox();
  expect(sidebar).not.toBeNull();
  expect(editor).not.toBeNull();
  expect(content).not.toBeNull();
  expect(terminal).not.toBeNull();
  expect(terminal!.x).toBeGreaterThanOrEqual(sidebar!.x + sidebar!.width);
  expect(Math.abs(terminal!.x - editor!.x)).toBeLessThan(1);
  expect(Math.abs(terminal!.width - editor!.width)).toBeLessThan(1);
  expect(terminal!.y).toBeGreaterThanOrEqual(editor!.y + editor!.height);

  await page.getByRole("button", { name: "Guide", exact: true }).click();
  const guide = await page.locator(".quickstart-sidebar").boundingBox();
  const editorWithGuide = await page.locator(".primary-surface").boundingBox();
  const terminalWithGuide = await page.locator(".terminal-dock").boundingBox();
  expect(guide).not.toBeNull();
  expect(editorWithGuide).not.toBeNull();
  expect(terminalWithGuide).not.toBeNull();
  expect(Math.abs(terminalWithGuide!.x - editorWithGuide!.x)).toBeLessThan(1);
  expect(Math.abs(terminalWithGuide!.width - editorWithGuide!.width)).toBeLessThan(1);
  expect(terminalWithGuide!.x + terminalWithGuide!.width).toBeLessThanOrEqual(guide!.x);
  await page.screenshot({ path: "test-results/terminal-editor-aligned-with-guide.png" });
  await page.getByRole("button", { name: "Hide guide", exact: true }).click();

  await files.filter({ hasText: "assets/sample.png" }).click();
  const image = page.locator(".binary-image-history img");
  await expect(image).toHaveCount(1);
  await expect(image).toBeVisible();
  await expect.poll(() => image.evaluate((node) => node.naturalWidth)).toBeGreaterThan(0);
  await page.screenshot({ path: "test-results/history-image-preview.png" });

  await page.screenshot({ path: "test-results/history-three-pane-terminal-span.png" });

  await page.locator(".segmented-control button").nth(0).click();
  await page.getByRole("button", { name: "Stage all", exact: true }).click();
  await expect(page.locator(".changes-section")).toHaveCount(1);
  await expect(page.locator(".changes-section")).toContainText("STAGED CHANGES");
  await expect(page.locator(".changes-section .change-row")).toHaveCount(2);

  await page.getByLabel("Commit message").fill("Commit staged worktree");
  await page.getByRole("button", { name: "Commit", exact: true }).click();
  await expect(page.locator(".empty-list")).toContainText("clean");
});

test("discard all rolls back every unstaged path after confirmation", async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem("graft-guide-open", "false");
    localStorage.setItem("graft-language", "en");
  });
  await seedRepository(page);
  await openVersionPanel(page);

  await page.getByRole("button", { name: "Discard all", exact: true }).click();
  const dialog = page.locator(".version-action-dialog");
  await expect(dialog).toBeVisible();
  await expect(dialog).toContainText("2 paths");
  await dialog.getByRole("button", { name: "Discard edits", exact: true }).click();

  await expect(page.locator(".changes-section")).toHaveCount(1);
  await expect(page.locator(".changes-section")).toContainText("STAGED CHANGES");
  await expect(page.locator(".changes-section .change-row")).toHaveCount(1);
});
