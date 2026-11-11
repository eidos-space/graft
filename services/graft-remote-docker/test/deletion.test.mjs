import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { Storage } from "../storage.mjs";
import { createApp } from "../app.mjs";

test("tombstone fences admitted uploads, waits for readers, persists and permanently reserves the name", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-delete-"));
  let storage = new Storage(join(root, "graft.sqlite"));
  t.after(() => {
    storage.close();
    rmSync(root, { recursive: true });
  });
  const row = storage.provision("notes", "Notes", true).row;
  const lease = storage.admit("notes");
  const backend = storage.backend(row);
  await backend.putIfAbsent("store/old", Buffer.from("old data"));
  let finish;
  const pending = backend.putIfAbsent(
    "store/pending",
    new ReadableStream({
      start(controller) {
        finish = () => {
          controller.enqueue(Buffer.from("late"));
          controller.close();
        };
      },
    }),
  );
  assert.equal(storage.requestDelete("notes").status, "deleting");
  assert.throws(() => storage.admit("notes"), { code: "repository_deleted" });
  await storage.cleanupStep("notes");
  assert.ok(backend.head("store/old"), "cleanup must wait for admitted readers");
  finish();
  await assert.rejects(pending, { code: "repository_deleted" });
  assert.throws(() => backend.compareAndSwap("refs/heads/main", undefined, Buffer.from("late")), {
    code: "repository_deleted",
  });
  lease.release();
  storage.close();
  storage = new Storage(join(root, "graft.sqlite"));
  assert.equal(storage.lookup("notes").status, "deleting");
  storage.db.exec(
    "CREATE TRIGGER fail_cleanup BEFORE DELETE ON objects BEGIN SELECT RAISE(ABORT, 'injected cleanup failure'); END",
  );
  await assert.rejects(storage.cleanupStep("notes"));
  assert.equal(storage.lookup("notes").status, "deleting");
  storage.db.exec("DROP TRIGGER fail_cleanup");
  await storage.cleanupStep("notes");
  assert.equal(storage.lookup("notes").status, "deleted");
  assert.equal(storage.requestDelete("notes").status, "deleted");
  assert.throws(() => storage.provision("notes", "Notes", true), {
    code: "repository_name_reserved",
  });
  const replacement = storage.provision("notes-new", "Notes", true).row;
  assert.notEqual(replacement.uuid, row.uuid);
  assert.equal(storage.backend(replacement).head("store/old"), null);
});

test("management capability, deletion status and authentication use the shared contract", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-delete-api-"));
  const storage = new Storage(join(root, "graft.sqlite"));
  t.after(() => {
    storage.close();
    rmSync(root, { recursive: true });
  });
  const token = randomBytes(32).toString("hex");
  const app = createApp({ storage, token, origin: "http://127.0.0.1:8787" });
  const request = (path, method = "GET", authorized = true) =>
    app(
      new Request(`http://127.0.0.1:8787${path}`, {
        method,
        headers: authorized ? { Authorization: `Bearer ${token}`, "Graft-Protocol": "1" } : {},
      }),
    );
  const discovery = await (await request("/.well-known/graft")).json();
  assert.equal(discovery.management.version, 1);
  assert.deepEqual(discovery.management.capabilities, ["rename", "delete"]);
  assert.equal((await request("/api/graft/repositories/test", "PUT")).status, 201);
  assert.equal((await request("/api/graft/repositories/test", "DELETE", false)).status, 401);
  const accepted = await request("/api/graft/repositories/test", "DELETE");
  assert.equal(accepted.status, 202);
  assert.equal((await accepted.json()).status, "deleting");
  assert.equal((await request("/personal/test")).status, 410);
  await storage.cleanupStep("test");
  const status = await request("/api/graft/repositories/test/status");
  assert.equal(status.status, 200);
  assert.equal((await status.json()).status, "deleted");
  assert.equal((await request("/api/graft/repositories/test", "DELETE")).status, 200);
  assert.equal((await request("/api/graft/repositories/test", "PUT")).status, 409);
  assert.deepEqual((await (await request("/api/graft/repositories")).json()).repositories, []);
});
