// Creates and removes only a unique Compose project, volume and temporary secret.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import {
  mkdtempSync,
  writeFileSync,
  rmSync,
  cpSync,
  mkdirSync,
  readFileSync,
  readdirSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createServer } from "node:net";
import { setTimeout } from "node:timers/promises";
import { sdkRoundTrip } from "./sdk-smoke.mjs";
import { proofHttp } from "./proof-http.mjs";

const directory = mkdtempSync(join(tmpdir(), "graft-container-"));
const project = `graft-test-${randomBytes(6).toString("hex")}`;
const token = randomBytes(32).toString("hex");
writeFileSync(join(directory, "key"), token, { mode: 0o444 });
const listener = createServer();
await new Promise((resolve) => listener.listen(0, "127.0.0.1", resolve));
const port = listener.address().port;
await new Promise((resolve) => listener.close(resolve));
const origin = `http://127.0.0.1:${port}`;
let sourceContext;
if (process.env.GRAFT_REMOTE_SOURCE) {
  sourceContext = join(directory, "source");
  mkdirSync(sourceContext);
  for (const file of [
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "LICENSE-APACHE",
    "LICENSE-MIT",
  ])
    cpSync(file, join(sourceContext, file));
  const remotePath = join(sourceContext, "packages/graft-remote");
  mkdirSync(remotePath, { recursive: true });
  for (const file of ["src", "package.json", "tsconfig.json", "tsconfig.build.json", "LICENSE"])
    cpSync(join(process.env.GRAFT_REMOTE_SOURCE, file), join(remotePath, file), {
      recursive: true,
    });
  const servicePath = join(sourceContext, "services/graft-remote-docker");
  mkdirSync(servicePath, { recursive: true });
  for (const file of readdirSync("services/graft-remote-docker").filter(
    (file) => file.endsWith(".mjs") || file.startsWith("Dockerfile") || ["REMOTE_VERSION", "REMOTE_SHA256"].includes(file),
  ))
    cpSync(join("services/graft-remote-docker", file), join(servicePath, file));
  console.log(
    JSON.stringify({
      remoteCandidate: resolve(process.env.GRAFT_REMOTE_SOURCE),
      version: JSON.parse(readFileSync(join(remotePath, "package.json"), "utf8")).version,
    }),
  );
}

writeFileSync(
  join(directory, "override.json"),
  JSON.stringify({
    services: {
      graft: {
        image: process.env.GRAFT_TEST_IMAGE ?? `${project}:local`,
        ...(sourceContext ? { build: { context: sourceContext, args: { GRAFT_REMOTE_BUILD: "source" } } } : {}),
        healthcheck: { interval: "1s", start_period: "1s" },
      },
    },
    secrets: { graft_access_key: { file: join(directory, "key") } },
  }),
);
const args = [
  "compose",
  "-p",
  project,
  "-f",
  resolve("services/graft-remote-docker/compose.yaml"),
  "-f",
  join(directory, "override.json"),
];
const env = { ...process.env, GRAFT_PUBLIC_ORIGIN: origin, GRAFT_PORT: String(port) };
const compose = (...command) =>
  execFileSync("docker", [...args, ...command], {
    env,
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 180_000,
  })
    .toString()
    .trim();
async function ready() {
  // Synchronous Docker calls delay socket-close notifications in this test process.
  await setTimeout(100);
  for (let i = 0; i < 60; i++) {
    try {
      if ((await fetch(`${origin}/healthz`, { headers: { Connection: "close" } })).ok) return;
    } catch {}
    await setTimeout(500);
  }
  try {
    const id = compose("ps", "-a", "-q", "graft");
    console.error(execFileSync("docker", ["inspect", "--format", "{{json .State}}", id], { encoding: "utf8" }));
  } catch {}
  throw new Error("Container failed to become ready");
}
async function request(path, options = {}) {
  assert.ok(path.startsWith("/") && !path.startsWith("//"));
  return fetch(`${origin}${path}`, {
    ...options,
    headers: {
      Connection: "close",
      Authorization: `Bearer ${token}`,
      "Graft-Protocol": "1",
      ...options.headers,
    },
    redirect: "error",
    signal: AbortSignal.timeout(30_000),
  });
}
async function restart() {
  const id = compose("ps", "-q", "graft");
  compose("kill", "-s", "SIGKILL", "graft");
  // Wait for the engine to publish the terminal state before Compose reconciles.
  execFileSync("docker", ["wait", id], { timeout: 30_000 });
  compose("up", "-d", "--wait", "--wait-timeout", "60");
  await ready();
}
try {
  compose("up", process.env.GRAFT_TEST_IMAGE ? "--no-build" : "--build", "-d", "--wait");
  await ready();
  const imageId = compose("images", "-q", "graft");
  const image = JSON.parse(
    execFileSync("docker", ["image", "inspect", imageId], { encoding: "utf8" }),
  )[0];
  console.log(
    JSON.stringify({
      imageId: image.Id,
      architecture: image.Architecture,
      sourceCommit: image.Config.Labels?.["org.opencontainers.image.revision"] ?? null,
    }),
  );
  console.log("Checking HTTP management");
  assert.equal((await fetch(`${origin}/api/graft/repositories`)).status, 401);
  assert.equal((await fetch(`${origin}/personal/missing`)).status, 401);
  assert.equal((await request("/personal/missing")).status, 404);
  const discovery = await (
    await fetch(`${origin}/.well-known/graft`, {
      headers: {
        Host: "evil.invalid",
        "X-Forwarded-Proto": "http",
        "X-Forwarded-Host": "evil.invalid",
      },
    })
  ).json();
  assert.equal(discovery.authentication.authority, origin);
  const create = await request("/api/graft/repositories/plain", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: '{"encryption":"plain"}',
  });
  assert.equal(create.status, 201);
  assert.equal(
    (
      await request("/personal/plain/raw-if-not-exists/store/test", {
        method: "PUT",
        body: "persistent data",
      })
    ).status,
    204,
  );
  assert.equal(
    (
      await request("/api/graft/repositories/oversized", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ display_name: "x".repeat(5000) }),
      })
    ).status,
    413,
  );
  console.log("Checking crash recovery");
  await restart();
  assert.equal(await (await request("/personal/plain/raw/store/test")).text(), "persistent data");
  // Recreate the container too: persistence must not depend on its writable layer.
  compose("up", "-d", "--force-recreate", "--wait");
  await ready();
  assert.equal(await (await request("/personal/plain/raw/store/test")).text(), "persistent data");
  compose("stop", "graft");
  const backup = execFileSync(
    "docker",
    [
      ...args,
      "run",
      "--rm",
      "--no-deps",
      "-T",
      "--entrypoint",
      "tar",
      "graft",
      "-C",
      "/data",
      "-czf",
      "-",
      ".",
    ],
    { env, timeout: 30_000 },
  );
  compose("down", "--volumes"); // Only this script's uniquely named test volume.
  execFileSync(
    "docker",
    [
      ...args,
      "run",
      "--rm",
      "--no-deps",
      "-T",
      "--entrypoint",
      "tar",
      "graft",
      "-C",
      "/data",
      "-xzf",
      "-",
    ],
    { env, input: backup, timeout: 30_000 },
  );
  compose("up", "-d", "--wait");
  await ready();
  assert.equal(await (await request("/personal/plain/raw/store/test")).text(), "persistent data");
  console.log(
    "PASS: Compose health, auth, canonical origin, SIGKILL, recreation, backup and empty-volume restore",
  );
  if (process.env.GRAFT_TEST_SKIP_SDK !== "1")
    await sdkRoundTrip({ origin, token, request, restart });
  if (process.env.GRAFT_CANDIDATE_SDK_PATH || process.env.GRAFT_CANDIDATE_NATIVE_PATH) {
    assert.equal(
      process.env.GRAFT_TEST_SKIP_SDK,
      "1",
      "Run candidate tests separately from the released SDK pass",
    );
    const { candidateRoundTrips } = await import("./candidate-smoke.mjs");
    const corruptCiphertext = (repository, path) => {
      const script = `
        const {DatabaseSync}=await import('node:sqlite');
        const fs=await import('node:fs');
        const db=new DatabaseSync('/data/graft.sqlite',{readOnly:true});
        const row=db.prepare('SELECT file,size FROM objects WHERE repository=? AND path=?').get(process.argv[1],process.argv[2]);
        if(!row?.file || row.size<=256) throw Error('Expected persisted multipart ciphertext');
        const fd=fs.openSync('/data/blobs/'+row.file,'r+');
        const b=Buffer.alloc(1); fs.readSync(fd,b,0,1,256); b[0]^=255;
        fs.writeSync(fd,b,0,1,256); fs.fsyncSync(fd);
        if(fs.fstatSync(fd).size!==row.size) throw Error('Length changed');
        fs.closeSync(fd); db.close();
        console.log(JSON.stringify({size:row.size,offset:256}));`;
      return JSON.parse(compose("exec", "-T", "graft", "node", "--input-type=module", "-e", script, repository, path));
    };
    await candidateRoundTrips({ origin, token, request, restart, corruptCiphertext });
  }
  if (process.env.GRAFT_LITE_CLIENT_PATH) {
    const output = execFileSync("bun", ["services/graft-remote-docker/test/lite-client.mjs"], {
      input: JSON.stringify({ origin, token, clientPath: process.env.GRAFT_LITE_CLIENT_PATH }),
      timeout: 30_000,
      encoding: "utf8",
    });
    console.log(output.trim());
  }
  await proofHttp({ request, compose });
  const removal = await request("/api/graft/repositories/plain", { method: "DELETE" });
  assert.equal(removal.status, 202);
  await restart();
  let removed = false;
  for (let attempt = 0; attempt < 20; attempt++) {
    const status = await request("/api/graft/repositories/plain/status");
    if ((await status.json()).status === "deleted") {
      removed = true;
      break;
    }
    await setTimeout(250);
  }
  assert.ok(removed, "deletion did not resume after restart");
  assert.equal((await request("/api/graft/repositories/plain", { method: "DELETE" })).status, 200);
  assert.equal((await request("/api/graft/repositories/plain", { method: "PUT" })).status, 409);
  assert.equal((await request("/personal/plain/raw/store/test")).status, 410);
  console.log("PASS: deletion after restart, idempotence, permanent name reservation");
  console.log("PASS: isolated container verification");
} catch (error) {
  console.error(compose("logs", "--no-color"));
  throw error;
} finally {
  try {
    compose("down", "--volumes", "--remove-orphans");
  } finally {
    rmSync(directory, { recursive: true });
    try {
      if (!process.env.GRAFT_TEST_IMAGE)
        execFileSync("docker", ["image", "rm", `${project}:local`], { stdio: "ignore" });
    } catch {}
  }
}
