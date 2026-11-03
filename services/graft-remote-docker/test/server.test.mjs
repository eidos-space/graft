import assert from "node:assert/strict";
import { test } from "node:test";
import { DatabaseSync } from "node:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { Storage } from "../storage.mjs";
import { createApp, validateOrigin, validateToken } from "../app.mjs";

const token = randomBytes(32).toString("hex");
const origin = "http://127.0.0.1:8787";
function setup(t) {
  const directory = mkdtempSync(join(tmpdir(), "graft-docker-test-"));
  let storage = new Storage(join(directory, "graft.sqlite"));
  let app = createApp({ storage, token, origin });
  t.after(() => {
    storage.close();
    rmSync(directory, { recursive: true });
  });
  return {
    get storage() {
      return storage;
    },
    restart() {
      storage.close();
      storage = new Storage(join(directory, "graft.sqlite"));
      app = createApp({ storage, token, origin });
    },
    request(path, options = {}, authenticated = true) {
      return app(
        new Request(`${origin}${path}`, {
          ...options,
          headers: {
            ...(authenticated ? { Authorization: `Bearer ${token}` } : {}),
            "Graft-Protocol": "1",
            ...options.headers,
          },
        }),
      );
    },
  };
}
const payload = (value) => ({
  method: "PUT",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify(value),
});

test("configuration rejects insecure origins and placeholder access keys", () => {
  for (const value of [
    "http://example.com",
    "https://example.com/path",
    "https://x:y@example.com",
    "https://example.com/?token=x",
  ])
    assert.throws(() => validateOrigin(value));
  assert.equal(validateOrigin("https://example.com/"), "https://example.com");
  assert.throws(() => validateToken("replace-me-with-a-secret-access-key"));
  assert.throws(() => validateToken("a".repeat(64)));
});

test("discovery, authentication, provisioning, rename, isolation and restart identity", async (t) => {
  const s = setup(t);
  assert.equal((await s.request("/healthz", {}, false)).status, 200);
  const discovery = await (await s.request("/.well-known/graft", {}, false)).json();
  assert.equal(discovery.management.repositories, `${origin}/api/graft/repositories`);
  for (const path of ["/api/graft/repositories", "/personal/missing"])
    assert.equal((await s.request(path, {}, false)).status, 401);
  assert.equal(
    (await s.request("/api/graft/repositories", { headers: { Authorization: "Bearer wrong" } }))
      .status,
    401,
  );
  assert.equal((await s.request("/personal/missing")).status, 404);
  const provision = await s.request(
    "/api/graft/repositories/notes",
    payload({ display_name: " 笔记 " }),
  );
  assert.equal(provision.status, 201);
  const first = await provision.json();
  assert.equal(first.display_name, "笔记");
  assert.equal(first.encryption.profile, "encrypted-remote-v1");
  const retries = await Promise.all(
    Array.from({ length: 12 }, () => s.request("/api/graft/repositories/notes", payload({}))),
  );
  for (const response of retries) {
    assert.equal(response.status, 200);
    assert.equal((await response.json()).encryption.uuid, first.encryption.uuid);
  }
  assert.equal(
    (await s.request("/api/graft/repositories/notes", payload({ encryption: "plain" }))).status,
    409,
  );
  assert.equal((await s.request("/other/notes")).status, 404);
  assert.equal(
    (
      await s.request("/api/graft/repositories/notes", {
        ...payload({ display_name: "New name" }),
        method: "PATCH",
      })
    ).status,
    200,
  );
  s.restart();
  const list = await (await s.request("/api/graft/repositories")).json();
  assert.equal(list.repositories[0].display_name, "New name");
  assert.deepEqual(list.repositories[0].encryption, first.encryption);
  const descriptor = await (await s.request("/personal/notes")).json();
  assert.deepEqual(descriptor.encryption, first.encryption);
  assert.ok(descriptor.capabilities.includes("e2ee-multipart-object"));
});

test("management rejects malformed, oversized and unknown fields", async (t) => {
  const s = setup(t);
  for (const value of [
    null,
    [],
    { password: "never accepted" },
    { display_name: "\n" },
    { encryption: "unknown" },
    { encryption: null },
    { display_name: null },
  ])
    assert.equal((await s.request("/api/graft/repositories/test", payload(value))).status, 400);
  assert.equal(
    (await s.request("/api/graft/repositories/test", payload({ display_name: "a".repeat(5000) })))
      .status,
    413,
  );
  assert.equal((await s.request("/api/graft/repositories/bad!", payload({}))).status, 400);
});

test("atomic create, CAS, compare-delete, ranges, pagination and object persistence", async (t) => {
  const s = setup(t);
  const row = s.storage.provision("test", "Test", false).row;
  let backend = s.storage.backend(row);
  const bytes = (value) => Buffer.from(value);
  const results = await Promise.all(
    Array.from({ length: 20 }, () => backend.putIfAbsent("store/a", bytes("original"))),
  );
  assert.equal(results.filter(Boolean).length, 1);
  assert.equal(backend.compareAndSwap("store/a", bytes("wrong"), bytes("oops")), false);
  assert.equal(backend.compareAndSwap("store/a", bytes("original"), bytes("updated")), true);
  assert.equal(backend.compareAndDelete("store/a", bytes("wrong")), false);
  await backend.putIfAbsent("store/b", bytes("second"));
  assert.deepEqual(backend.list({ prefix: "store/", limit: 1 }).paths, ["store/a"]);
  assert.equal(backend.list({ prefix: "store/", limit: 1 }).hasMore, true);
  assert.deepEqual(backend.list({ prefix: "store/", after: "store/a", limit: 1 }).paths, [
    "store/b",
  ]);
  s.restart();
  backend = s.storage.backend(s.storage.lookup("test"));
  assert.equal(Buffer.from(backend.get("store/a").body).toString(), "updated");
  const response = await s.request("/personal/test/raw/store/a", {
    headers: { Range: "bytes=1-3" },
  });
  assert.equal(response.status, 206);
  assert.equal(await response.text(), "pda");
  assert.equal(
    (
      await s.request("/personal/test/raw-if-not-exists/store/a", {
        method: "PUT",
        body: "different",
      })
    ).status,
    412,
  );
  assert.equal(backend.compareAndDelete("store/a", bytes("updated")), true);
  assert.equal(backend.compareAndDelete("store/a", undefined), true);
});

test("interrupted object stream never publishes a partial object or changes an existing object", async (t) => {
  const s = setup(t);
  const backend = s.storage.backend(s.storage.provision("interrupted", "Interrupted", false).row);
  const broken = () =>
    new ReadableStream({
      start(controller) {
        controller.enqueue(Buffer.from("partial"));
      },
      pull(controller) {
        controller.error(new Error("synthetic transport interruption"));
      },
    });
  await assert.rejects(backend.putIfAbsent("store/new", broken()));
  assert.equal(backend.head("store/new"), null);
  await backend.putIfAbsent("store/existing", Buffer.from("original"));
  await assert.rejects(backend.putIfAbsent("store/existing", broken()));
  assert.equal(Buffer.from(backend.get("store/existing").body).toString(), "original");
  s.restart();
  const restored = s.storage.backend(s.storage.lookup("interrupted"));
  assert.equal(restored.head("store/new"), null);
  assert.equal(Buffer.from(restored.get("store/existing").body).toString(), "original");
});

test("independent SQLite connections agree on atomic creation and ref CAS", async (t) => {
  const directory = mkdtempSync(join(tmpdir(), "graft-docker-concurrency-"));
  const path = join(directory, "graft.sqlite");
  const first = new Storage(path);
  const second = new Storage(path);
  t.after(() => {
    first.close();
    second.close();
    rmSync(directory, { recursive: true });
  });
  const row = first.provision("atomic", "Atomic", false).row;
  const left = first.backend(row),
    right = second.backend(row);
  const value = Buffer.from("first");
  const results = await Promise.all([
    left.putIfAbsent("store/object", value),
    right.putIfAbsent("store/object", Buffer.from("other")),
  ]);
  assert.equal(results.filter(Boolean).length, 1);
  assert.equal(left.compareAndSwap("refs/heads/main", undefined, value), true);
  assert.equal(right.compareAndSwap("refs/heads/main", undefined, Buffer.from("wrong")), false);
  assert.equal(right.compareAndSwap("refs/heads/main", value, Buffer.from("second")), true);
  assert.equal(left.compareAndSwap("refs/heads/main", value, Buffer.from("stale")), false);
  assert.equal(Buffer.from(left.get("refs/heads/main").body).toString(), "second");
});

test("schema-1 upgrade preserves identities and objects; future schema fails before journal changes", (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-migrate-"));
  const file = join(root, "graft.sqlite");
  const old = new DatabaseSync(file);
  old.exec(`CREATE TABLE repositories(name TEXT PRIMARY KEY,display_name TEXT NOT NULL,created_at INTEGER NOT NULL,uuid TEXT UNIQUE);
    CREATE TABLE objects(repository TEXT NOT NULL REFERENCES repositories(name),path TEXT COLLATE BINARY NOT NULL,value BLOB NOT NULL,PRIMARY KEY(repository,path)) WITHOUT ROWID;
    INSERT INTO repositories VALUES('old','Old',123,NULL);
    INSERT INTO objects VALUES('old','store/data',X'010203'); PRAGMA user_version=1;`);
  old.close();
  const storage = new Storage(file);
  assert.equal(storage.lookup("old").status, "active");
  assert.equal(storage.lookup("old").created_at, 123);
  assert.deepEqual(
    Buffer.from(storage.backend(storage.lookup("old")).get("store/data").body),
    Buffer.from([1, 2, 3]),
  );
  assert.equal(storage.db.prepare("PRAGMA user_version").get().user_version, 4);
  storage.close();
  const future = new DatabaseSync(file);
  future.exec("PRAGMA journal_mode=DELETE; PRAGMA user_version=999;");
  future.close();
  assert.throws(() => new Storage(file), /Unsupported storage version/);
  const verify = new DatabaseSync(file);
  assert.equal(verify.prepare("PRAGMA journal_mode").get().journal_mode, "delete");
  verify.close();
  rmSync(root, { recursive: true });
});
