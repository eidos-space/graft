import { expect, test, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";

const graftCliManifest = readFileSync(
  new URL("../../../crates/graft-cli/Cargo.toml", import.meta.url),
  "utf8",
);
const graftVersion = graftCliManifest.match(/^version = "([^"]+)"$/m)?.[1];
if (!graftVersion) throw new Error("Could not read the graft-cli version");

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

async function runRaw(page: Page, args: string[]) {
  return page.evaluate(async (command) => {
    const client = (
      window as typeof window & {
        graftTestClient: {
          run(args: string[]): Promise<CommandResult>;
        };
      }
    ).graftTestClient;
    return client.run(command);
  }, args);
}

async function runPlaygroundRaw(page: Page, args: string[]) {
  return page.evaluate(async (command) => {
    const client = (
      window as typeof window & {
        graftPlaygroundClient: {
          run(args: string[]): Promise<CommandResult>;
        };
      }
    ).graftPlaygroundClient;
    return client.run(command);
  }, args);
}

async function playgroundMergeApi<T>(page: Page, args: string[]) {
  const result = await runPlaygroundRaw(page, ["merge-api", ...args]);
  expect(result.code, result.stderr.join("\n")).toBe(0);
  return JSON.parse(result.stdout.join("\n")) as T;
}

async function mergeApi<T>(page: Page, args: string[]) {
  const result = await runRaw(page, ["merge-api", ...args]);
  expect(result.code, result.stderr.join("\n")).toBe(0);
  return JSON.parse(result.stdout.join("\n")) as T;
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

test("runtime and version views expose first-class path renames", async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem("graft-guide-open", "false");
    localStorage.setItem("graft-language", "en");
  });
  await page.goto("/e2e.html?reset=1");
  await expect(page.locator("html")).toHaveAttribute("data-ready", "true");
  await run(page, ["init"]);
  await writeOpfs(page, "notes.txt", "path identity stays intact\n");
  await run(page, ["add", "--all"]);
  await run(page, ["commit", "-m", "Add notes"]);
  await run(page, [
    "--browser-cwd",
    "/",
    "browser-move",
    "/notes.txt",
    "/archive-notes.txt",
  ]);
  await run(page, ["add", "--all"]);

  await openVersionPanel(page);
  await expect(page.locator(".graft-version")).toHaveAttribute(
    "data-graft-version",
    graftVersion,
  );
  const rename = page.locator(".changes-section .change-row");
  await expect(rename).toHaveCount(1);
  await expect(rename).toContainText("notes.txt → archive-notes.txt");
  await expect(rename.locator(".change-renamed")).toHaveText("R");
  await rename.locator(".change-main").click();
  await expect(page.locator(".diff-surface")).toBeVisible();

  await page.getByLabel("Commit message").fill("Archive notes");
  await page.getByRole("button", { name: "Commit", exact: true }).click();
  await expect(page.locator(".empty-list")).toContainText("clean");
  await page.locator(".segmented-control button").nth(1).click();
  await page.locator(".history-entry > button").first().click();
  const committedRename = page.locator(".commit-file-list > button");
  await expect(committedRename).toHaveCount(1);
  await expect(committedRename).toContainText("notes.txt → archive-notes.txt");
  await expect(committedRename.locator(".change-renamed")).toHaveText("R");
});

test("WASM merge-api plans up-to-date, fast-forward, and three-way histories", async ({ page }) => {
  const apiPage = await page.context().newPage();
  await apiPage.goto("/e2e.html?reset=1");
  await expect(apiPage.locator("html")).toHaveAttribute("data-ready", "true");
  await run(apiPage, ["init"]);
  const branchList = JSON.parse(
    (await run(apiPage, ["branch", "--json"])).stdout.join("\n"),
  ) as { branches: Array<{ current: boolean; name: string }> };
  const initialBranch = branchList.branches.find((branch) => branch.current)?.name ?? "main";
  await writeOpfs(apiPage, "merge-plan.txt", "base\n");
  await run(apiPage, ["add", "--all"]);
  await run(apiPage, ["commit", "-m", "plan base"]);

  await run(apiPage, ["switch", "-c", "merge-plan/target"]);
  await writeOpfs(apiPage, "merge-plan.txt", "target\n");
  await run(apiPage, ["add", "--all"]);
  await run(apiPage, ["commit", "-m", "plan target"]);
  await run(apiPage, ["switch", initialBranch]);

  const fastForward = await mergeApi<{ kind: string }>(apiPage, [
    "plan",
    "merge-plan/target",
  ]);
  expect(fastForward.kind).toBe("fast_forward");

  const upToDate = await mergeApi<{ kind: string }>(apiPage, ["plan", initialBranch]);
  expect(upToDate.kind).toBe("up_to_date");

  await writeOpfs(apiPage, "merge-plan.txt", "local\n");
  await run(apiPage, ["add", "--all"]);
  await run(apiPage, ["commit", "-m", "plan local"]);
  const threeWay = await mergeApi<{ kind: string; plan_token: string }>(apiPage, [
    "plan",
    "merge-plan/target",
  ]);
  expect(threeWay.kind).toBe("three_way");
  expect(threeWay.plan_token).toMatch(/.+/);
  await apiPage.close();
});

test("merge lab keeps durable text and SQLite conflicts recoverable", async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem("graft-guide-open", "false");
    localStorage.setItem("graft-language", "en");
  });
  // Seed before opening the Playground so the harness worker is torn down on
  // navigation. A second long-lived WASM worker can race the UI's OPFS handles.
  await page.goto("/e2e.html?reset=1");
  await expect(page.locator("html")).toHaveAttribute("data-ready", "true");
  await run(page, ["init"]);

  await openVersionPanel(page);
  await page.getByRole("button", { name: "Merge lab", exact: true }).click();
  await expect(page.locator('option[value="merge-lab/theirs"]')).toHaveCount(1, {
    timeout: 30_000,
  });

  await page.locator(".branch-merge-toggle").click();
  await page.locator(".branch-merge select").selectOption("merge-lab/theirs");
  await page.locator(".branch-merge button").click();
  await expect(page.locator(".conflict-workspace")).toBeVisible({ timeout: 30_000 });
  await expect(page.locator(".conflict-lineage")).toContainText("LOCAL");
  await expect(page.locator(".conflict-lineage")).toContainText("HOSTED");
  await expect(page.locator(".conflict-lineage")).toContainText("BASE");
  await expect(page.locator(".conflict-progress code")).toContainText("state");
  await page.locator(".conflict-paths button").filter({ hasText: "fixture.txt" }).click();
  await expect(page.locator(".conflict-text-review")).toBeVisible();
  await expect(page.locator(".conflict-row-comparison")).toHaveCount(0);

  const mergeStatus = await playgroundMergeApi<{
    state: "merging";
    state_token: string;
    unmerged_count: number;
  }>(page, ["status"]);
  expect(mergeStatus.state).toBe("merging");
  expect(mergeStatus.unmerged_count).toBeGreaterThanOrEqual(2);

  const stale = await runPlaygroundRaw(page, [
    "merge-api",
    "path",
    "merge-lab/fixture.txt",
    "ours",
    "--state-token",
    "stale-token",
  ]);
  expect(stale.code).not.toBe(0);
  expect(`${stale.stderr.join("\n")} ${stale.stdout.join("\n")}`).toMatch(/token|state|stale/i);

  await page.screenshot({ path: "test-results/merge-conflict-workbench.png", fullPage: true });
  const resultEditor = page.locator(".conflict-result-editor textarea");
  await expect(resultEditor).toHaveValue(/ours line/);
  await resultEditor.fill("resolved result\nkeep this context\n");
  await page.getByRole("button", { name: "Save result", exact: true }).click();
  await expect(
    page.locator(".conflict-paths button").filter({ hasText: "fixture.txt" }),
  ).toContainText("0 unresolved");

  await page.locator(".conflict-paths button").filter({ hasText: "fixture.sqlite" }).click();
  await expect(page.locator(".conflict-row-comparison")).toBeVisible();
  await expect(page.locator(".conflict-row-comparison")).toContainText("ours row");
  await expect(page.locator(".conflict-row-comparison")).toContainText("theirs row");
  const chooseOurs = page.getByRole("button", { name: "Choose ours", exact: true });
  await expect(chooseOurs).toBeEnabled();
  await chooseOurs.click();
  await expect(
    page.locator(".conflict-paths button").filter({ hasText: "fixture.sqlite" }),
  ).toContainText("0 unresolved", { timeout: 30_000 });
  await expect(page.getByRole("button", { name: "Finish merge", exact: true })).toBeVisible();

  const resolvedStatus = await playgroundMergeApi<{
    state: "merging";
    unmerged_count: number;
  }>(page, ["status"]);
  expect(resolvedStatus.state).toBe("merging");
  expect(resolvedStatus.unmerged_count).toBe(0);

  await page.reload();
  await expect(page.locator(".conflict-workspace")).toBeVisible({ timeout: 30_000 });
  await expect(page.locator(".conflict-progress code")).toContainText("state");
  await expect(page.getByRole("button", { name: "Finish merge", exact: true })).toBeVisible();
  await page.screenshot({ path: "test-results/merge-recovered-after-reload.png", fullPage: true });

  await page.getByLabel("Merge commit message").fill("Merge lab resolution");
  await page.getByRole("button", { name: "Finish merge", exact: true }).click();
  await expect(page.locator(".conflict-workspace")).toHaveCount(0);
  await expect(page.locator(".version-panel")).toContainText("Merge lab resolution");
});

test("merge lab applies whole-path sides and aborts back to the original head", async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem("graft-guide-open", "false");
    localStorage.setItem("graft-language", "en");
  });
  await page.goto("/e2e.html?reset=1");
  await expect(page.locator("html")).toHaveAttribute("data-ready", "true");
  await run(page, ["init"]);
  await openVersionPanel(page);
  await page.getByRole("button", { name: "Merge lab", exact: true }).click();
  await expect(page.locator('option[value="merge-lab/theirs"]')).toHaveCount(1, {
    timeout: 30_000,
  });
  await page.locator(".branch-merge-toggle").click();
  await page.locator(".branch-merge select").selectOption("merge-lab/theirs");
  await page.locator(".branch-merge button").click();
  await expect(page.locator(".conflict-workspace")).toBeVisible({ timeout: 30_000 });

  await page.getByRole("button", { name: "Use all ours", exact: true }).click();
  await expect(
    page.locator(".conflict-paths button").filter({ hasText: "fixture.sqlite" }),
  ).toContainText("0 unresolved", { timeout: 30_000 });
  await page.locator(".conflict-paths button").filter({ hasText: "fixture.txt" }).click();
  await page.getByRole("button", { name: "Use all theirs", exact: true }).click();
  await expect(
    page.locator(".conflict-paths button").filter({ hasText: "fixture.txt" }),
  ).toContainText("0 unresolved", { timeout: 30_000 });

  const resolved = await playgroundMergeApi<{ state: "merging"; unmerged_count: number }>(
    page,
    ["status"],
  );
  expect(resolved.state).toBe("merging");
  expect(resolved.unmerged_count).toBe(0);
  await page.getByRole("button", { name: "Abort merge", exact: true }).click();
  await expect(page.locator(".conflict-workspace")).toHaveCount(0);
  await expect(page.locator('.branch-bar select')).toHaveValue("merge-lab/ours");
  const aborted = await playgroundMergeApi<{ state: string }>(page, ["status"]);
  expect(aborted.state).toBe("none");
});

test("history expands files inside each commit and keeps one full-width diff inspector", async ({
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
  await expect(page.locator(".commit-file-list > button")).toHaveCount(0);
  await page.locator(".history-entry > button").first().click();
  const expandedCommit = page.locator(".history-entry").first();
  await expect(expandedCommit.locator(".history-entry-toggle")).toHaveAttribute(
    "aria-expanded",
    "true",
  );
  const files = expandedCommit.locator(".commit-file-list > button");
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
  expect(Math.abs(content!.x - editor!.x)).toBeLessThan(1);
  expect(Math.abs(content!.width - editor!.width)).toBeLessThan(1);
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

  await files.filter({ hasText: "README.md" }).click();
  await expect(page.locator(".version-inspector")).toBeVisible();
  await expect(page.locator(".diff-inspector-breadcrumb")).toContainText(
    "History›README.md",
  );
  await expect(page.getByRole("button", { name: "Split", exact: true })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  await page.getByRole("button", { name: "Unified", exact: true }).click();
  await expect(page.getByRole("button", { name: "Unified", exact: true })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  await page.screenshot({ path: "test-results/history-inline-files-text-diff.png" });

  await files.filter({ hasText: "assets/sample.png" }).click();
  const image = page.locator(".binary-image-history img");
  await expect(image).toHaveCount(1);
  await expect(image).toBeVisible();
  await expect.poll(() => image.evaluate((node) => node.naturalWidth)).toBeGreaterThan(0);
  await page.screenshot({ path: "test-results/history-image-preview.png" });

  await page.screenshot({ path: "test-results/history-inline-files-image-diff.png" });

  await expandedCommit.locator(".history-entry-toggle").click();
  await expect(expandedCommit.locator(".commit-file-list > button")).toHaveCount(0);
  await expect(page.locator(".version-inspector")).toHaveCount(0);

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
