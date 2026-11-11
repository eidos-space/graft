// Explicit opt-in for a newly created disposable R2 bucket, never a production bucket.
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  S3Client,
  HeadBucketCommand,
  ListObjectsV2Command,
  DeleteObjectsCommand,
  ListMultipartUploadsCommand,
  AbortMultipartUploadCommand,
} from "@aws-sdk/client-s3";

export async function startR2Fixture(directory, manifest) {
  const config = JSON.parse(readFileSync(manifest, "utf8"));
  assert.equal(config.kind, "graft-docker-disposable-r2-v1");
  assert.equal(config.allowDestructiveTest, true);
  assert.match(config.bucket, /^graft-docker-test-\d{8}-[a-f0-9]{10}$/);
  assert.match(config.endpoint, /^https:\/\/[a-f0-9]{32}\.r2\.cloudflarestorage\.com$/);
  assert.equal(config.region, "auto");
  assert.equal(config.prefix, "acceptance/");
  const credentials = {
    accessKeyId: readFileSync(config.accessKeyIdFile, "utf8").trim(),
    secretAccessKey: readFileSync(config.secretAccessKeyFile, "utf8").trim(),
  };
  const client = new S3Client({
    endpoint: config.endpoint,
    region: config.region,
    credentials,
    requestChecksumCalculation: "WHEN_REQUIRED",
    responseChecksumValidation: "WHEN_REQUIRED",
    maxAttempts: 2,
  });
  const send = (command) => client.send(command, { abortSignal: AbortSignal.timeout(60_000) });
  const Bucket = config.bucket;
  try {
    await send(new HeadBucketCommand({ Bucket }));
    const initial = await send(new ListObjectsV2Command({ Bucket, MaxKeys: 1 }));
    assert.equal(initial.Contents?.length ?? 0, 0, "Disposable test bucket must start empty");
    const uploads = await send(new ListMultipartUploadsCommand({ Bucket }));
    assert.equal(uploads.Uploads?.length ?? 0, 0, "Disposable bucket must have no old uploads");
    writeFileSync(join(directory, "s3-id"), credentials.accessKeyId, { mode: 0o444 });
    writeFileSync(join(directory, "s3-key"), credentials.secretAccessKey, { mode: 0o444 });
    console.log(
      JSON.stringify({
        provider: "Cloudflare R2",
        bucket: Bucket,
        prefix: config.prefix,
        emptyAtStart: true,
      }),
    );
    return {
      service: {
        environment: {
          GRAFT_STORAGE: "s3",
          GRAFT_S3_ENDPOINT: config.endpoint,
          GRAFT_S3_REGION: "auto",
          GRAFT_S3_BUCKET: Bucket,
          GRAFT_S3_PREFIX: config.prefix,
          GRAFT_S3_ACCESS_KEY_ID_FILE: "/run/secrets/s3_id",
          GRAFT_S3_SECRET_ACCESS_KEY_FILE: "/run/secrets/s3_key",
        },
        secrets: ["s3_id", "s3_key"],
      },
      secrets: {
        s3_id: { file: join(directory, "s3-id") },
        s3_key: { file: join(directory, "s3-key") },
      },
      async close() {
        let deleted = 0,
          aborted = 0;
        try {
          // R2 has no object versioning. Remove only this fixture's exclusive prefix.
          for (;;) {
            const page = await send(new ListObjectsV2Command({ Bucket, MaxKeys: 1000 }));
            const objects = page.Contents ?? [];
            if (!objects.length) break;
            assert.ok(
              objects.every(({ Key }) => Key.startsWith(config.prefix)),
              "Unexpected key outside test prefix",
            );
            const result = await send(
              new DeleteObjectsCommand({
                Bucket,
                Delete: { Objects: objects.map(({ Key }) => ({ Key })) },
              }),
            );
            assert.equal(result.Errors?.length ?? 0, 0, "R2 test cleanup failed");
            deleted += objects.length;
          }
          for (;;) {
            const page = await send(new ListMultipartUploadsCommand({ Bucket }));
            const uploads = page.Uploads ?? [];
            if (!uploads.length) break;
            for (const { Key, UploadId } of uploads) {
              assert.ok(Key.startsWith(config.prefix), "Unexpected upload outside test prefix");
              await send(new AbortMultipartUploadCommand({ Bucket, Key, UploadId }));
              aborted++;
            }
          }
          console.log(
            JSON.stringify({
              provider: "Cloudflare R2",
              bucket: Bucket,
              cleanupObjects: deleted,
              cleanupUploads: aborted,
              emptyAtEnd: true,
            }),
          );
        } finally {
          client.destroy();
        }
      },
    };
  } catch (error) {
    client.destroy();
    throw error;
  }
}
