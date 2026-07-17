import { expect, test, type Page } from "@playwright/test";

interface CommandResult {
  code: number;
  stderr: string[];
  stdout: string[];
}

async function run(page: Page, args: string[]): Promise<CommandResult> {
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

async function openHarness(page: Page, reset = false) {
  await page.goto(`/e2e.html${reset ? "?reset=1" : ""}`);
  await expect(page.locator("html")).toHaveAttribute("data-ready", "true");
}

async function queryValue(page: Page, path: string, sql: string) {
  const result = await run(page, ["--db", path, "sql", sql]);
  return result.stdout.join("\n");
}

async function opfsFileExists(page: Page, path: string) {
  return page.evaluate(async (candidate) => {
    const parts = candidate.split("/").filter(Boolean);
    let directory = await navigator.storage.getDirectory();
    try {
      for (const part of parts.slice(0, -1)) {
        directory = await directory.getDirectoryHandle(part);
      }
      await directory.getFileHandle(parts.at(-1)!);
      return true;
    } catch (error) {
      if (error instanceof DOMException && error.name === "NotFoundError") return false;
      throw error;
    }
  }, path);
}

test("naked switch rebinds every SQLite snapshot and survives an OPFS reload", async ({
  page,
}) => {
  await openHarness(page, true);
  await run(page, ["init"]);
  await run(page, ["status", "--json"]);
  expect(await opfsFileExists(page, ".graft/control.sqlite")).toBe(false);
  expect(await opfsFileExists(page, ".graft-clone.sqlite")).toBe(false);
  await run(page, [
    "--db",
    "primary.sqlite",
    "sql",
    "CREATE TABLE notes(body TEXT); INSERT INTO notes VALUES ('main primary');",
  ]);
  await run(page, [
    "--db",
    "secondary.sqlite",
    "sql",
    "CREATE TABLE settings(value TEXT); INSERT INTO settings VALUES ('main secondary');",
  ]);
  await run(page, ["add", "--all"]);
  await run(page, ["commit", "-m", "main workspace"]);

  await run(page, ["switch", "-c", "feature"]);
  await run(page, [
    "--db",
    "primary.sqlite",
    "sql",
    "UPDATE notes SET body = 'feature primary';",
  ]);
  await run(page, [
    "--db",
    "secondary.sqlite",
    "sql",
    "UPDATE settings SET value = 'feature secondary';",
  ]);
  await run(page, ["add", "--all"]);
  await run(page, ["commit", "-m", "feature workspace"]);

  await run(page, ["switch", "main"]);
  expect(await queryValue(page, "primary.sqlite", "SELECT body FROM notes;")).toContain(
    "main primary",
  );
  expect(
    await queryValue(page, "secondary.sqlite", "SELECT value FROM settings;"),
  ).toContain("main secondary");
  const status = JSON.parse((await run(page, ["status", "--json"])).stdout.join("\n"));
  expect(status.current_branch).toBe("main");
  expect(status.dirty).toBe(false);

  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-ready", "true");
  expect(await queryValue(page, "primary.sqlite", "SELECT body FROM notes;")).toContain(
    "main primary",
  );
  expect(
    await queryValue(page, "secondary.sqlite", "SELECT value FROM settings;"),
  ).toContain("main secondary");
});
