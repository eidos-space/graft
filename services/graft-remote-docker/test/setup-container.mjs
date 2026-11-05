// Only uniquely named test containers, networks and volumes are created/removed.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { startS3Fixture } from "./s3-fixture.mjs";
import { sdkRoundTrip } from "./sdk-smoke.mjs";

const image = process.env.GRAFT_TEST_IMAGE;
if (!image) throw new Error("Set GRAFT_TEST_IMAGE to the candidate image");
const docker = (...args) =>
  execFileSync("docker", args, {
    encoding: "utf8",
    timeout: 180000,
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
for (const mode of ["local", ...(process.env.GRAFT_S3_TEST_IMAGE ? ["s3"] : [])]) {
  const name = `graft-web-setup-${randomBytes(6).toString("hex")}`;
  const directory = mkdtempSync(join(tmpdir(), name));
  let fixture;
  let origin;
  const ready = async () => {
    for (let i = 0; i < 100; i++) {
      try {
        if ((await fetch(origin + "/livez")).ok) return;
      } catch {}
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error("Setup server did not start");
  };
  try {
    if (mode === "s3") fixture = await startS3Fixture(directory, name);
    // Reserve a published port using a stopped container, then recreate with its origin.
    docker("run", "-d", "--name", name, "-p", "127.0.0.1::8787", "-v", `${name}:/data`, image);
    const port = docker("port", name, "8787/tcp").split(":").at(-1);
    origin = `http://127.0.0.1:${port}`;
    docker("rm", "-f", name);
    const run = () =>
      docker(
        "run",
        "-d",
        "--name",
        name,
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "-p",
        `127.0.0.1:${port}:8787`,
        "-e",
        `GRAFT_PUBLIC_ORIGIN=${origin}`,
        "-v",
        `${name}:/data`,
        ...(fixture ? ["--network", `${name}-s3-net`, "-e", "GRAFT_S3_ALLOW_HTTP=true"] : []),
        image,
      );
    run();
    await ready();
    const link = docker(
      "exec",
      name,
      "node",
      "services/graft-remote-docker/manage.mjs",
      "setup-link",
    );
    const claim = new URLSearchParams(new URL(link).hash.slice(1)).get("token");
    assert.ok(claim);
    assert.equal((await fetch(origin + "/healthz")).status, 503);
    const input =
      mode === "local"
        ? { mode }
        : {
            mode,
            endpoint: fixture.service.environment.GRAFT_S3_ENDPOINT,
            region: "us-east-1",
            bucket: "graft-test",
            prefix: "web-setup/",
            pathStyle: true,
            accessKeyId: readFileSync(join(directory, "s3-id"), "utf8"),
            secretAccessKey: readFileSync(join(directory, "s3-key"), "utf8"),
          };
    const post = (value) => ({
      method: "POST",
      headers: {
        Origin: origin,
        "Content-Type": "application/json",
        Authorization: `Bearer ${claim}`,
      },
      body: JSON.stringify(value),
    });
    if (mode === "s3") {
      assert.equal(
        (await fetch(origin + "/setup/test", post({ ...input, secretAccessKey: "wrong" }))).status,
        400,
      );
      assert.equal((await fetch(origin + "/healthz")).status, 503);
    }
    const checked = await fetch(origin + "/setup/test", post(input));
    assert.equal(checked.status, 200, await checked.text());
    const result = await fetch(origin + "/setup", post(input));
    assert.equal(result.status, 200, result.status === 200 ? undefined : await result.text());
    const { accessKey } = await result.json();
    assert.equal((await fetch(origin + "/healthz")).status, 200);
    const auth = { Authorization: `Bearer ${accessKey}` };
    const created = await fetch(origin + "/api/graft/repositories/setup-test", {
      method: "PUT",
      headers: { ...auth, "Content-Type": "application/json" },
      body: JSON.stringify({ display_name: "Setup test", encryption: "plain" }),
    });
    assert.equal(created.status, 201);
    docker("rm", "-f", name);
    run();
    await ready();
    assert.equal((await fetch(origin + "/setup", post(input))).status, 409);
    assert.equal(
      docker("exec", name, "node", "services/graft-remote-docker/manage.mjs", "access-key"),
      accessKey,
    );
    const list = await (await fetch(origin + "/api/graft/repositories", { headers: auth })).json();
    assert.equal(list.repositories[0].name, "setup-test");
    if (process.env.GRAFT_SDK_PATH) {
      await sdkRoundTrip({
        origin,
        token: accessKey,
        request: (path, options = {}) =>
          fetch(origin + path, {
            ...options,
            headers: { ...auth, "Graft-Protocol": "1", ...options.headers },
          }),
        restart: async () => {
          docker("rm", "-f", name);
          run();
          await ready();
        },
      });
    }
    const logs = docker("logs", name);
    assert.ok(!logs.includes(accessKey) && !logs.includes(claim));
    console.log(
      JSON.stringify({
        mode,
        setup: "passed",
        recreate: "passed",
        keyRecovery: "passed",
        secretLogs: "absent",
      }),
    );
  } finally {
    try {
      docker("rm", "-f", name);
    } catch {}
    try {
      docker("volume", "rm", name);
    } catch {}
    fixture?.close();
    rmSync(directory, { recursive: true });
  }
}
