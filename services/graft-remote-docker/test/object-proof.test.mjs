import { test } from "node:test";
import assert from "node:assert/strict";
import { randomBytes, createHash } from "node:crypto";
import { mkdtempSync, writeFileSync, truncateSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { Storage } from "../storage.mjs";

// Prefix construction/HTTP vectors belong to the shared handler suite. These
// tests isolate the backend's required actual persisted snapshot behavior.
test("proof hashes actual persisted bytes, changes per challenge and rejects truncation", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-object-proof-"));
  const storage = new Storage(join(root, "graft.sqlite"));
  t.after(() => {
    storage.close();
    rmSync(root, { recursive: true });
  });
  const row = storage.provision("proof", "Proof", true).row;
  const lease = storage.admit(row.name),
    backend = storage.backend(row);
  const bytes = randomBytes(2 * 1024 * 1024),
    prefix = randomBytes(128);
  const file = "test-immutable-file";
  const path = `e2ee/files/${"a".repeat(64)}`;
  writeFileSync(join(storage.blobDirectory, file), bytes);
  storage.db.prepare("INSERT INTO blob_files VALUES(?,?)").run(file, row.name);
  storage.db
    .prepare("INSERT INTO objects(repository,path,value,file,size) VALUES(?,?,?,?,?)")
    .run(row.name, path, Buffer.alloc(0), file, bytes.length);
  const expected = createHash("sha256").update(prefix).update(bytes).digest("hex");
  const proof = await backend.encryptedObjectProof(path, prefix);
  assert.deepEqual(proof, { size: bytes.length, sha256: expected });
  assert.notEqual(
    (await backend.encryptedObjectProof(path, randomBytes(128))).sha256,
    proof.sha256,
  );
  const damaged = Buffer.from(bytes);
  damaged[damaged.length - 1] ^= 1;
  writeFileSync(join(storage.blobDirectory, file), damaged);
  assert.notEqual(
    (await backend.encryptedObjectProof(path, prefix)).sha256,
    expected,
    "cached upload digest would hide corruption",
  );
  truncateSync(join(storage.blobDirectory, file), bytes.length - 1);
  await assert.rejects(backend.encryptedObjectProof(path, prefix), {
    code: "object_length_mismatch",
  });
  writeFileSync(join(storage.blobDirectory, file), bytes);
  const pending = backend.encryptedObjectProof(path, prefix);
  storage.requestDelete(row.name);
  await storage.cleanupStep(row.name);
  assert.throws(() => storage.admit(row.name), { code: "repository_deleted" });
  assert.deepEqual(
    await pending,
    proof,
    "admitted proof must read the same immutable file through deletion",
  );
  lease.release();
  await storage.cleanupStep(row.name);
  await storage.cleanupStep(row.name);
  assert.equal(await backend.encryptedObjectProof(path, prefix), null);
});

test("proof honors cancellation and bounded concurrency; plain repositories never opt in", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-object-proof-cancel-"));
  const storage = new Storage(join(root, "graft.sqlite"));
  t.after(() => {
    storage.close();
    rmSync(root, { recursive: true });
  });
  const row = storage.provision("encrypted", "Encrypted", true).row;
  const backend = storage.backend(row, { signal: AbortSignal.abort() });
  await assert.rejects(backend.encryptedObjectProof("e2ee/objects/a", Buffer.alloc(32)), {
    name: "AbortError",
  });
  assert.equal(storage.proofsActive, 0);
  const file = "concurrent-proof-file";
  writeFileSync(join(storage.blobDirectory, file), randomBytes(2 * 1024 * 1024));
  storage.db
    .prepare("INSERT INTO objects(repository,path,value,file,size) VALUES(?,?,?,?,?)")
    .run(row.name, "e2ee/files/actual", Buffer.alloc(0), file, 2 * 1024 * 1024);
  const active = storage.backend(row);
  const first = active.encryptedObjectProof("e2ee/files/actual", Buffer.alloc(32));
  const second = active.encryptedObjectProof("e2ee/files/actual", Buffer.alloc(32));
  await assert.rejects(active.encryptedObjectProof("e2ee/files/actual", Buffer.alloc(32)), {
    code: "object_proof_busy",
  });
  await Promise.all([first, second]);
  const cancellation = new AbortController();
  const pending = storage
    .backend(row, { signal: cancellation.signal })
    .encryptedObjectProof("e2ee/files/actual", Buffer.alloc(32));
  cancellation.abort();
  await assert.rejects(pending, { name: "AbortError" });
  assert.equal(storage.proofsActive, 0);
  assert.equal(
    storage.backend(storage.provision("plain", "Plain", false).row).encryptedObjectProof,
    undefined,
  );
});
