import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { S3Client, CreateBucketCommand, PutBucketVersioningCommand } from "@aws-sdk/client-s3";

export async function startS3Fixture(directory, project) {
  const image = process.env.GRAFT_S3_TEST_IMAGE;
  if (!image) throw new Error("GRAFT_S3_TEST_IMAGE is required for isolated S3 tests");
  const name = `${project}-s3`;
  const network = `${project}-s3-net`;
  const credentials = {
    accessKeyId: randomBytes(12).toString("hex"),
    secretAccessKey: randomBytes(24).toString("hex"),
  };
  const environment = join(directory, "s3.env");
  writeFileSync(
    environment,
    `MINIO_ROOT_USER=${credentials.accessKeyId}\nMINIO_ROOT_PASSWORD=${credentials.secretAccessKey}\n`,
    { mode: 0o600 },
  );
  writeFileSync(join(directory, "s3-id"), credentials.accessKeyId, {
    mode: 0o444,
  });
  writeFileSync(join(directory, "s3-key"), credentials.secretAccessKey, {
    mode: 0o444,
  });
  const close = () => {
    try {
      execFileSync("docker", ["rm", "-f", "-v", name], { stdio: "pipe" });
    } catch {}
    try {
      execFileSync("docker", ["network", "rm", network], { stdio: "pipe" });
    } catch {}
  };
  let client;
  try {
    execFileSync("docker", ["network", "create", network], { stdio: "pipe" });
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
        "--network",
        network,
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
      .split("\n")[0]
      .split(":")
      .at(-1);
    const endpoint = `http://127.0.0.1:${port}`;
    client = new S3Client({
      endpoint,
      region: "us-east-1",
      forcePathStyle: true,
      credentials,
      requestChecksumCalculation: "WHEN_REQUIRED",
    });
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
    if (!ready) throw Error("S3 fixture failed to start");
    await client.send(new CreateBucketCommand({ Bucket: "graft-test" }));
    await client.send(
      new PutBucketVersioningCommand({
        Bucket: "graft-test",
        VersioningConfiguration: { Status: "Enabled" },
      }),
    );
    return {
      close,
      service: {
        networks: ["s3_test"],
        environment: {
          GRAFT_STORAGE: "s3",
          GRAFT_S3_ENDPOINT: `http://${name}:9000`,
          GRAFT_S3_ALLOW_HTTP: "true",
          GRAFT_S3_REGION: "us-east-1",
          GRAFT_S3_BUCKET: "graft-test",
          GRAFT_S3_PREFIX: "isolated/",
          GRAFT_S3_ACCESS_KEY_ID_FILE: "/run/secrets/s3_id",
          GRAFT_S3_SECRET_ACCESS_KEY_FILE: "/run/secrets/s3_key",
        },
        secrets: ["s3_id", "s3_key"],
      },
      networks: { s3_test: { external: true, name: network } },
      secrets: {
        s3_id: { file: join(directory, "s3-id") },
        s3_key: { file: join(directory, "s3-key") },
      },
    };
  } catch (error) {
    close();
    throw error;
  } finally {
    client?.destroy();
  }
}
