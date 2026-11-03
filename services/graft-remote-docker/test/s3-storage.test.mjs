import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash, randomBytes } from "node:crypto";
import { Storage } from "../storage.mjs";
import { S3Blobs, s3Configuration } from "../s3.mjs";

class MemoryBlobs {
  identity = "s3:test-bucket";
  objects = new Map();
  async put(id, bytes) {
    this.objects.set(id, Buffer.from(bytes));
  }
  async upload(id, stream) {
    const chunks = [];
    for await (const chunk of stream) chunks.push(chunk);
    await this.put(id, Buffer.concat(chunks));
  }
  async get(id, range) {
    const bytes = this.objects.get(id);
    if (!bytes) throw new Error("Missing persisted object");
    const body = range ? bytes.subarray(range.start, range.end + 1) : bytes;
    return { size: body.length, body: new Response(body).body };
  }
  async delete(id) {
    this.objects.delete(id);
  }
  close() {}
}
function setup(t) {
  const root = mkdtempSync(join(tmpdir(), "graft-s3-test-"));
  const blobs = new MemoryBlobs();
  const storage = new Storage(join(root, "graft.sqlite"), { blobStore: blobs });
  t.after(() => {
    if (!storage.closed) storage.close();
    rmSync(root, { recursive: true });
  });
  const row = storage.provision("test", "Test", true).row;
  return { root, storage, blobs, row, backend: storage.backend(row) };
}

test("S3-backed publication keeps control metadata local, ranges and proof read actual bytes", async (t) => {
  const { storage, blobs, backend } = setup(t);
  const key = `e2ee/files/${"a".repeat(64)}`;
  const bytes = Buffer.from("actual ciphertext bytes");
  assert.equal(await backend.putIfAbsent(key, bytes), true);
  assert.equal(await backend.putIfAbsent(key, Buffer.from("different")), false);
  const persisted = storage.db.prepare("SELECT * FROM objects WHERE path=?").get(key);
  assert.equal(persisted.value.length, 0);
  assert.deepEqual(
    Buffer.from(
      await new Response((await backend.get(key, { start: 2, end: 8 })).body).arrayBuffer(),
    ),
    bytes.subarray(2, 9),
  );
  assert.equal(await backend.putIfAbsent("e2ee/config", Buffer.from("config")), true);
  assert.equal(
    backend.compareAndSwap("e2ee/config", Buffer.from("config"), Buffer.from("updated")),
    true,
  );
  const prefix = Buffer.from("challenge");
  const proof = await backend.encryptedObjectProof(key, prefix);
  assert.equal(proof.sha256, createHash("sha256").update(prefix).update(bytes).digest("hex"));
  blobs.objects.get(persisted.file)[0] ^= 255;
  assert.notEqual((await backend.encryptedObjectProof(key, prefix)).sha256, proof.sha256);
  blobs.objects.set(persisted.file, bytes.subarray(1));
  await assert.rejects(backend.encryptedObjectProof(key, prefix), {
    code: "object_length_mismatch",
  });
});

test("failed and tombstoned S3 writes never publish; cleanup retries remote failures", async (t) => {
  const { storage, blobs, backend } = setup(t);
  const original = blobs.put.bind(blobs);
  let finish;
  blobs.put = async (...args) => {
    await original(...args);
    await new Promise((resolve) => {
      finish = resolve;
    });
  };
  const pending = backend.putIfAbsent("store/pending", Buffer.from("complete upload"));
  while (!finish) await new Promise((resolve) => setImmediate(resolve));
  storage.requestDelete("test");
  await storage.cleanupStep("test");
  assert.equal(blobs.objects.size, 1, "in-flight upload must not be collected");
  finish();
  await assert.rejects(pending, { code: "repository_deleted" });
  assert.equal(backend.head("store/pending"), null);
  blobs.delete = async () => {
    throw new Error("remote unavailable");
  };
  await assert.rejects(storage.cleanupStep("test"));
  assert.equal(storage.lookup("test").status, "deleting");
  blobs.delete = MemoryBlobs.prototype.delete;
  await storage.cleanupStep("test");
  await storage.cleanupStep("test");
  assert.equal(blobs.objects.size, 0);
  assert.equal(storage.lookup("test").status, "deleted");
});

test("S3 multipart survives reopening and failed completion, removes local final blob after publication", async (t) => {
  const { root, storage, blobs, row } = setup(t);
  const bytes = randomBytes(1024 * 1024 + 80);
  bytes.write("GRAFTEO1");
  bytes.writeUInt16BE(1, 8);
  bytes.writeUInt16BE(1, 10);
  bytes.writeUInt32BE(16, 68);
  const identity = {
    uuid: row.uuid,
    uploadId: bytes.subarray(12, 44).toString("hex"),
    ciphertextSha256: createHash("sha256").update(bytes).digest("hex"),
  };
  const key = `e2ee/files/${"b".repeat(64)}`;
  const initial = storage.backend(row).multipart.start(key, bytes.length, 1024 * 1024, identity);
  await storage
    .backend(row)
    .multipart.uploadPart(key, initial.uploadId, 1, bytes.subarray(0, 1024 * 1024), 1024 * 1024);
  storage.close();
  const reopened = new Storage(join(root, "graft.sqlite"), {
    blobStore: blobs,
  });
  t.after(() => reopened.close());
  const backend = reopened.backend(row);
  assert.equal(
    backend.multipart.start(key, bytes.length, 1024 * 1024, identity).uploadedParts.length,
    1,
  );
  await backend.multipart.uploadPart(key, initial.uploadId, 2, bytes.subarray(1024 * 1024), 80);
  const upload = blobs.upload.bind(blobs);
  blobs.upload = async (...args) => {
    await upload(...args);
    throw new Error("lost upload response");
  };
  await assert.rejects(backend.multipart.complete(key, initial.uploadId));
  assert.equal(backend.head(key), null);
  blobs.upload = upload;
  assert.equal(await backend.multipart.complete(key, initial.uploadId), true);
  assert.deepEqual(readdirSync(join(root, "blobs")), []);
  assert.deepEqual(
    Buffer.from(await new Response((await backend.get(key)).body).arrayBuffer()),
    bytes,
  );
  assert.equal(reopened.db.prepare("SELECT count(*) n FROM parts").get().n, 0);
});

test("storage mode and bucket identity cannot silently change; credentials remain explicit files", (t) => {
  const { root, storage, blobs } = setup(t);
  assert.equal(storage.db.prepare("PRAGMA user_version").get().user_version, 5);
  storage.close();
  assert.throws(() => new Storage(join(root, "graft.sqlite")), /differs/);
  assert.throws(
    () =>
      new Storage(join(root, "graft.sqlite"), {
        blobStore: { ...blobs, identity: "s3:other" },
      }),
    /differs/,
  );
  assert.equal(s3Configuration({}), null);
  assert.throws(() => s3Configuration({ GRAFT_STORAGE: "wrong" }), /local or s3/);
  const key = join(root, "key");
  const secret = join(root, "secret");
  writeFileSync(key, "test-id");
  writeFileSync(secret, "test-secret");
  const env = {
    GRAFT_STORAGE: "s3",
    GRAFT_S3_ENDPOINT: "https://s3.us-west-004.backblazeb2.com",
    GRAFT_S3_BUCKET: "test-bucket",
    GRAFT_S3_REGION: "us-west-004",
    GRAFT_S3_ACCESS_KEY_ID_FILE: key,
    GRAFT_S3_SECRET_ACCESS_KEY_FILE: secret,
  };
  assert.equal(s3Configuration(env).credentials.secretAccessKey, "test-secret");
  assert.throws(
    () =>
      s3Configuration({
        ...env,
        GRAFT_S3_ENDPOINT: "https://secret:password@example.com",
      }),
    /origin/,
  );
  assert.throws(() => s3Configuration({ ...env, GRAFT_S3_PREFIX: "../" }), /PREFIX/);
});

test("S3 errors are structured and invalid range responses are rejected without exposing provider details", async () => {
  const blobs = new S3Blobs({
    endpoint: "https://example.invalid",
    region: "test",
    bucket: "test-bucket",
    prefix: "graft/",
    credentials: { accessKeyId: "test-id", secretAccessKey: "test-secret" },
  });
  blobs.client.destroy();
  const id = "00000000-0000-0000-0000-000000000000";
  blobs.client = {
    send: async () => {
      throw {
        $metadata: { httpStatusCode: 404 },
        message: "private provider details",
      };
    },
  };
  await assert.rejects(blobs.get(id), {
    status: 404,
    code: "object_not_found",
  });
  blobs.client.send = async () => {
    throw {
      $metadata: { httpStatusCode: 403 },
      message: "private provider details",
    };
  };
  await assert.rejects(
    blobs.get(id),
    (error) => error.status === 503 && !error.message.includes("private"),
  );
  let destroyed = false;
  blobs.client.send = async () => ({
    Body: {
      destroy: () => {
        destroyed = true;
      },
    },
    ContentLength: 10,
    ContentRange: "bytes 0-9/100",
  });
  await assert.rejects(blobs.get(id, { start: 0, end: 3 }), {
    status: 409,
    code: "object_length_mismatch",
  });
  assert.equal(destroyed, true);
  // Correct selected bytes with a false total must not be accepted as a header probe.
  destroyed = false;
  await assert.rejects(blobs.get(id, { start: 0, end: 9 }, undefined, 101), {
    status: 409,
    code: "object_length_mismatch",
  });
  assert.equal(destroyed, true);
  await assert.rejects(blobs.get(id, undefined, undefined, 101), {
    status: 409,
    code: "object_length_mismatch",
  });
});
