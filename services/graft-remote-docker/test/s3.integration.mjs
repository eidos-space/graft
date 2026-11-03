// Explicit isolated MinIO integration. Never accepts an existing bucket or real credentials.
import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, rmSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes, createHash } from "node:crypto";
import {
  S3Client,
  CreateBucketCommand,
  PutBucketVersioningCommand,
  PutObjectCommand,
  ListObjectsV2Command,
  ListObjectVersionsCommand,
} from "@aws-sdk/client-s3";
import { Storage } from "../storage.mjs";
import { S3Blobs } from "../s3.mjs";
import { createApp } from "../app.mjs";

const image = process.env.GRAFT_S3_TEST_IMAGE;
if (!image)
  throw new Error(
    "Set GRAFT_S3_TEST_IMAGE to an available MinIO test image; no production endpoint is accepted",
  );

test(
  "real versioned S3: restart, multipart publication, HTTP proof, corruption, ranges and tombstone cleanup",
  { timeout: 180_000 },
  async () => {
    const root = mkdtempSync(join(tmpdir(), "graft-s3-integration-"));
    const name = `graft-s3-${randomBytes(6).toString("hex")}`;
    const credentials = {
      accessKeyId: randomBytes(12).toString("hex"),
      secretAccessKey: randomBytes(24).toString("hex"),
    };
    const environment = join(root, "minio.env");
    writeFileSync(
      environment,
      `MINIO_ROOT_USER=${credentials.accessKeyId}\nMINIO_ROOT_PASSWORD=${credentials.secretAccessKey}\n`,
      { mode: 0o600 },
    );
    let storage, client;
    try {
      execFileSync(
        "docker",
        [
          "run",
          "-d",
          "--name",
          name,
          "--pull=never",
          "-p",
          "127.0.0.1::9000",
          "--env-file",
          environment,
          image,
          "server",
          "/data",
        ],
        { stdio: "pipe" },
      );
      const port = execFileSync("docker", ["port", name, "9000/tcp"], {
        encoding: "utf8",
      })
        .trim()
        .split(":")
        .at(-1);
      const endpoint = `http://127.0.0.1:${port}`;
      let ready = false;
      for (let i = 0; i < 60; i++) {
        try {
          if ((await fetch(`${endpoint}/minio/health/live`)).ok) {
            ready = true;
            break;
          }
        } catch {}
        await new Promise((resolve) => setTimeout(resolve, 250));
      }
      assert.ok(ready);
      const config = {
        endpoint,
        region: "us-east-1",
        bucket: "graft-test",
        prefix: "isolated/",
        forcePathStyle: true,
        credentials,
      };
      client = new S3Client({
        ...config,
        requestChecksumCalculation: "WHEN_REQUIRED",
      });
      await client.send(new CreateBucketCommand({ Bucket: config.bucket }));
      await client.send(
        new PutBucketVersioningCommand({
          Bucket: config.bucket,
          VersioningConfiguration: { Status: "Enabled" },
        }),
      );
      const open = () =>
        new Storage(join(root, "graft.sqlite"), {
          blobStore: new S3Blobs(config),
        });
      storage = open();
      await storage.blobStore.health();
      const row = storage.provision("repository", "S3 test", true).row;
      let backend = storage.backend(row);
      const token = randomBytes(32).toString("hex");
      const app = () => createApp({ storage, origin: "http://127.0.0.1:8787", token });
      const key = `e2ee/files/${"a".repeat(64)}`;
      const bytes = randomBytes(20 * 1024 * 1024 + 123);
      bytes.write("GRAFTEO1");
      bytes.writeUInt16BE(1, 8);
      bytes.writeUInt16BE(1, 10);
      bytes.writeUInt32BE(16, 68);
      const identity = {
        uuid: row.uuid,
        uploadId: bytes.subarray(12, 44).toString("hex"),
        ciphertextSha256: createHash("sha256").update(bytes).digest("hex"),
      };
      const partBytes = 16 * 1024 * 1024;
      const upload = backend.multipart.start(key, bytes.length, partBytes, identity);
      await backend.multipart.uploadPart(
        key,
        upload.uploadId,
        1,
        bytes.subarray(0, partBytes),
        partBytes,
      );
      storage.close();
      storage = open();
      backend = storage.backend(row);
      assert.equal(
        backend.multipart.start(key, bytes.length, partBytes, identity).uploadedParts.length,
        1,
      );
      await backend.multipart.uploadPart(
        key,
        upload.uploadId,
        2,
        bytes.subarray(partBytes),
        bytes.length - partBytes,
      );
      assert.equal(await backend.multipart.complete(key, upload.uploadId), true);
      assert.deepEqual(readdirSync(join(root, "blobs")), []);
      const object = storage.db
        .prepare("SELECT * FROM objects WHERE repository=? AND path=?")
        .get(row.name, key);
      assert.equal(object.value.length, 0);
      assert.equal(storage.db.prepare("SELECT count(*) n FROM parts").get().n, 0);
      assert.deepEqual(
        Buffer.from(await new Response((await backend.get(key)).body).arrayBuffer()),
        bytes,
      );
      assert.deepEqual(
        Buffer.from(
          await new Response(
            (
              await backend.get(key, {
                start: partBytes - 10,
                end: partBytes + 10,
              })
            ).body,
          ).arrayBuffer(),
        ),
        bytes.subarray(partBytes - 10, partBytes + 11),
      );
      const challenge = randomBytes(32);
      const proofRequest = () =>
        new Request(`http://127.0.0.1:8787/personal/repository/e2ee/object-proof/${key}`, {
          method: "POST",
          headers: {
            Authorization: `Bearer ${token}`,
            "Graft-Protocol": "1",
            "Content-Type": "application/octet-stream",
            "Content-Length": "32",
            "X-Graft-Object-Bytes": String(bytes.length),
          },
          body: challenge,
        });
      const original = await (await app()(proofRequest())).json();
      assert.ok(original.sha256);
      // A versioned provider preserves the old data; proof must read the actual newest snapshot.
      const damaged = Buffer.from(bytes);
      damaged[256] ^= 255;
      await client.send(
        new PutObjectCommand({
          Bucket: config.bucket,
          Key: storage.blobStore.key(object.file),
          Body: damaged,
        }),
      );
      assert.notEqual((await (await app()(proofRequest())).json()).sha256, original.sha256);
      await client.send(
        new PutObjectCommand({
          Bucket: config.bucket,
          Key: storage.blobStore.key(object.file),
          Body: bytes.subarray(0, 100),
        }),
      );
      assert.equal((await app()(proofRequest())).status, 409);
      // A repository reader admission pins deletion while the stream is consumed.
      const lease = storage.admit(row.name);
      storage.requestDelete(row.name);
      assert.equal((await app()(proofRequest())).status, 404);
      await storage.cleanupStep(row.name);
      assert.ok(storage.backend(row).head(key));
      lease.release();
      storage.close();
      storage = open();
      for (let i = 0; i < 5 && storage.lookup(row.name).status !== "deleted"; i++)
        await storage.cleanupStep(row.name);
      assert.equal(storage.lookup(row.name).status, "deleted");
      const remaining = await client.send(
        new ListObjectsV2Command({
          Bucket: config.bucket,
          Prefix: config.prefix,
        }),
      );
      assert.equal(remaining.KeyCount, 0);
      const versions = await client.send(
        new ListObjectVersionsCommand({
          Bucket: config.bucket,
          Prefix: config.prefix,
        }),
      );
      assert.equal(
        versions.Versions?.length ?? 0,
        0,
        "physical cleanup must remove versioned bytes",
      );
      assert.equal(versions.DeleteMarkers?.length ?? 0, 0);
      console.log(
        JSON.stringify({
          provider: "isolated MinIO",
          image,
          multipartBytes: bytes.length,
          range: true,
          restart: true,
          proofActualContent: true,
          truncationRejected: true,
          versionsDeleted: true,
        }),
      );
    } finally {
      storage?.close();
      client?.destroy();
      try {
        execFileSync("docker", ["rm", "-f", "-v", name], { stdio: "pipe" });
      } catch {}
      rmSync(root, { recursive: true });
    }
  },
);
