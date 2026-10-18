// Exercise the packaged executable, not a development web server.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";

if (!process.argv[2]) throw new Error("Usage: node smoke-cli.mjs PATH_TO_GRAFT");
const binary = resolve(process.argv[2]);
const root = await mkdtemp(join(tmpdir(), "graft-webui-smoke-"));
let child;
try {
  await writeFile(join(root, "note.html"), "<h1>Before</h1>\n");
  const database = new DatabaseSync(join(root, "data.sqlite"));
  database.exec(
    "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes VALUES(1, 'before')",
  );
  database.close();
  for (const args of [["init"], ["add", "."], ["commit", "-m", "WebUI smoke fixture"]]) {
    const result = spawnSync(binary, args, { cwd: root, encoding: "utf8", timeout: 30000 });
    assert.equal(result.status, 0, result.stderr || result.error?.message);
  }
  await writeFile(join(root, "note.html"), "<h1>Literal source after edit</h1>\n");
  const changed = new DatabaseSync(join(root, "data.sqlite"));
  changed.exec("UPDATE notes SET body='after' WHERE id=1");
  changed.close();
  child = spawn(binary, [root, "--no-open"], { stdio: ["ignore", "pipe", "pipe"] });
  let output = "";
  let errors = "";
  child.stderr.on("data", (chunk) => {
    errors += chunk;
  });
  const url = await new Promise((fulfill, reject) => {
    const timer = setTimeout(() => reject(new Error("WebUI startup timed out")), 30000);
    child.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`WebUI exited ${code}: ${errors}`));
    });
    child.stdout.on("data", (chunk) => {
      output += chunk;
      const match = output.match(/http:\/\/127\.0\.0\.1:\d+\/#token=[a-f0-9]+/);
      if (match) {
        clearTimeout(timer);
        fulfill(new URL(match[0]));
      }
    });
  });
  const token = new URLSearchParams(url.hash.slice(1)).get("token");
  const request = async (endpoint, input) => {
    const response = await fetch(new URL(`/api/${endpoint}`, url), {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: `Bearer ${token}` },
      body: JSON.stringify(input),
      signal: AbortSignal.timeout(30000),
    });
    assert.equal(response.status, 200, `${endpoint}: ${await response.clone().text()}`);
    return response.json();
  };
  const page = await fetch(url, { signal: AbortSignal.timeout(30000) });
  assert.equal(page.status, 200);
  assert.match(await page.text(), /app\.js/);
  const app = await fetch(new URL("/app.js", url));
  assert.equal(app.status, 200);
  assert.ok((await app.arrayBuffer()).byteLength > 1000);
  const snapshot = await request("snapshot", {});
  assert.ok(snapshot.paths.includes("note.html"));
  assert.ok(snapshot.databases.includes("data.sqlite"));
  const file = await request("file", { path: "note.html", view: "file" });
  assert.equal(file.after.state, "utf8");
  assert.match(file.after.content, /<h1>Literal source/);
  const rows = await request("database", { path: "data.sqlite", table: "notes" });
  assert.deepEqual(rows.rows, [[1, "after"]]);
  const diff = await request("sqlite-diff", { path: "data.sqlite", table: "notes" });
  assert.equal(diff.tables[0].rowid_alias, "id");
  assert.deepEqual(diff.tables[0].primary_key_columns, ["id"]);
  assert.equal(diff.summaries[0].updates, 1);
  const denied = await fetch(new URL("/api/snapshot", url), { method: "POST", body: "{}" });
  assert.equal(denied.status, 403);
  console.log(
    "Packaged WebUI PASS: embedded UI, text source, SQLite rows/diff, session authorization",
  );
} finally {
  if (child?.pid && child.exitCode === null && child.signalCode === null) {
    const exit = once(child, "exit");
    child.kill();
    await exit;
  }
  await rm(root, { recursive: true, force: true });
}
