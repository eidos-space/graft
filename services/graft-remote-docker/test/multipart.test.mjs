import { test } from "node:test";
import assert from "node:assert/strict";
import { randomBytes, createHash } from "node:crypto";
import { mkdtempSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { Storage } from "../storage.mjs";
const stream = (bytes) => new Response(bytes).body;

test("encrypted multipart survives reopening, verifies identity/digest and atomically publishes a streamed file", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-multipart-"));
  let storage = new Storage(join(root, "graft.sqlite"));
  t.after(() => {
    storage.close();
    rmSync(root, { recursive: true });
  });
  const row = storage.provision("large", "Large", true).row;
  const bytes = randomBytes(1024 * 1024 + 123);
  bytes.write("GRAFTEO1");
  bytes.writeUInt16BE(1, 8);
  bytes.writeUInt16BE(1, 10);
  bytes.writeUInt32BE(16, 68);
  const encryption = {
    uuid: row.uuid,
    uploadId: bytes.subarray(12, 44).toString("hex"),
    ciphertextSha256: createHash("sha256").update(bytes).digest("hex"),
  };
  const path = `e2ee/files/${"a".repeat(64)}`;
  let backend = storage.backend(row);
  const started = backend.multipart.start(path, bytes.length, 512 * 1024, encryption);
  assert.throws(() =>
    backend.multipart.start(path, bytes.length, 512 * 1024, {
      ...encryption,
      ciphertextSha256: "0".repeat(64),
    }),
  );
  await backend.multipart.uploadPart(
    path,
    started.uploadId,
    1,
    stream(bytes.subarray(0, 512 * 1024)),
    512 * 1024,
  );
  assert.equal(backend.head(path), null);
  await assert.rejects(backend.multipart.complete(path, started.uploadId));
  storage.close();
  storage = new Storage(join(root, "graft.sqlite"));
  backend = storage.backend(storage.lookup("large"));
  const resumed = backend.multipart.start(path, bytes.length, 512 * 1024, encryption);
  assert.equal(resumed.uploadId, started.uploadId);
  assert.equal(resumed.uploadedParts.length, 1);
  for (let number = 2; number <= 3; number++) {
    const part = bytes.subarray((number - 1) * 512 * 1024, number * 512 * 1024);
    await backend.multipart.uploadPart(path, resumed.uploadId, number, stream(part), part.length);
  }
  assert.equal(await backend.multipart.complete(path, resumed.uploadId), true);
  assert.equal(await backend.multipart.complete(path, resumed.uploadId), false);
  assert.equal(backend.head(path).size, bytes.length);
  assert.deepEqual(Buffer.from(await new Response(backend.get(path).body).arrayBuffer()), bytes);
  assert.deepEqual(
    Buffer.from(await new Response(backend.get(path, { start: 100, end: 200 }).body).arrayBuffer()),
    bytes.subarray(100, 201),
  );
  storage.close();
  storage = new Storage(join(root, "graft.sqlite"));
  backend = storage.backend(storage.lookup("large"));
  assert.deepEqual(Buffer.from(await new Response(backend.get(path).body).arrayBuffer()), bytes);
  storage.requestDelete("large");
  await storage.cleanupStep("large");
  await storage.cleanupStep("large");
  assert.equal(storage.lookup("large").status, "deleted");
});

test("multipart corruption and abort never expose an object", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-multipart-bad-"));
  const storage = new Storage(join(root, "graft.sqlite"));
  t.after(() => {
    storage.close();
    rmSync(root, { recursive: true });
  });
  const row = storage.provision("test", "Test", true).row,
    backend = storage.backend(row);
  const path = `e2ee/files/${"b".repeat(64)}`;
  const identity = { uuid: row.uuid, uploadId: "a".repeat(64), ciphertextSha256: "b".repeat(64) };
  const upload = backend.multipart.start(path, 104, 104, identity);
  await backend.multipart.uploadPart(path, upload.uploadId, 1, stream(Buffer.alloc(104)), 104);
  await assert.rejects(backend.multipart.complete(path, upload.uploadId), {
    code: "invalid_encrypted_ciphertext",
  });
  assert.equal(backend.head(path), null);
  await backend.multipart.abort(path, upload.uploadId);
  assert.equal(backend.multipart.start(path, 104, 104, identity).uploadedParts.length, 0);
});

test("idle expired multipart is reclaimed, but active admissions protect resumable work", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-multipart-expiry-"));
  const storage = new Storage(join(root, "graft.sqlite"));
  t.after(() => {
    storage.close();
    rmSync(root, { recursive: true });
  });
  const row = storage.provision("expiry", "Expiry", false).row;
  const backend = storage.backend(row);
  const upload = backend.multipart.start("store/large", 100, 100);
  await backend.multipart.uploadPart(
    "store/large",
    upload.uploadId,
    1,
    stream(Buffer.alloc(100)),
    100,
  );
  const lease = storage.admit(row.name);
  storage.db.prepare("UPDATE uploads SET updated=0 WHERE id=?").run(upload.uploadId);
  await new Promise((resolve) => setTimeout(resolve, 1100));
  assert.equal(storage.db.prepare("SELECT COUNT(*) AS count FROM uploads").get().count, 1);
  lease.release();
  await new Promise((resolve) => setTimeout(resolve, 1100));
  assert.equal(storage.db.prepare("SELECT COUNT(*) AS count FROM uploads").get().count, 0);
  assert.equal(storage.db.prepare("SELECT COUNT(*) AS count FROM parts").get().count, 0);
  assert.equal(backend.head("store/large"), null);
});
