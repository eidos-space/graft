import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, readFileSync, existsSync, statSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { createService } from "../setup.mjs";

const origin = "http://127.0.0.1:8787";
async function fixture(t, options = {}) {
  const directory = mkdtempSync(join(tmpdir(), "graft-setup-"));
  let service = await createService({ directory, origin, env: {}, ...options });
  t.after(() => {
    service.close();
    rmSync(directory, { recursive: true });
  });
  return {
    directory,
    claim: () => readFileSync(join(directory, "setup-token"), "utf8"),
    request: (path, init = {}) => service.request(new Request(origin + path, init)),
    async restart() {
      service.close();
      service = await createService({ directory, origin, env: {}, ...options });
    },
  };
}
function post(token, value = { mode: "local" }, extra = {}) {
  return {
    method: "POST",
    body: JSON.stringify(value),
    headers: {
      Origin: origin,
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
      ...extra,
    },
  };
}

test("new server exposes setup, not sync; claim survives restart without leaking into HTML", async (t) => {
  const s = await fixture(t);
  const claim = s.claim();
  assert.equal(statSync(join(s.directory, "setup-token")).mode & 0o777, 0o600);
  assert.equal((await s.request("/livez")).status, 200);
  assert.equal((await s.request("/healthz")).status, 503);
  assert.equal((await s.request("/api/graft/repositories")).status, 503);
  const page = await s.request("/setup");
  assert.equal(page.headers.get("cache-control"), "no-store");
  assert.match(page.headers.get("content-security-policy"), /frame-ancestors 'none'/);
  assert.ok(!(await page.text()).includes(claim));
  await s.restart();
  assert.equal(s.claim(), claim);
});

test("setup rejects unauthenticated, cross-origin, null-origin, large and malformed submissions", async (t) => {
  const s = await fixture(t);
  assert.equal((await s.request("/setup", post("wrong"))).status, 401);
  for (const Origin of ["null", "https://evil.example", ""])
    assert.equal((await s.request("/setup", post(s.claim(), undefined, { Origin }))).status, 403);
  assert.equal((await s.request("/setup", post(s.claim(), { mode: "invalid" }))).status, 400);
  assert.equal(
    (await s.request("/setup", post(s.claim(), { mode: "local", pad: "x".repeat(17000) }))).status,
    413,
  );
  assert.equal((await s.request("/setup", { ...post(s.claim()), body: "{" })).status, 400);
  assert.ok(!existsSync(join(s.directory, "server-config.json")));
});

test("local setup creates one persistent key, rejects replay, authenticates sync after restart", async (t) => {
  const s = await fixture(t);
  const claim = s.claim();
  const response = await s.request("/setup", post(claim));
  assert.equal(response.status, 200);
  const { accessKey } = await response.json();
  assert.match(accessKey, /^[a-f0-9]{64}$/);
  assert.equal(statSync(join(s.directory, "server-config.json")).mode & 0o777, 0o600);
  assert.ok(!existsSync(join(s.directory, "setup-token")));
  assert.equal((await s.request("/setup", post(claim))).status, 409);
  assert.equal((await s.request("/healthz")).status, 200);
  const auth = { headers: { Authorization: `Bearer ${accessKey}` } };
  assert.equal((await s.request("/api/graft/repositories", auth)).status, 200);
  assert.equal((await s.request("/api/graft/repositories")).status, 401);
  await s.restart();
  assert.equal((await s.request("/api/graft/repositories", auth)).status, 200);
  assert.equal((await s.request("/setup", post(claim))).status, 409);
  assert.ok(!(await (await s.request("/setup")).text()).includes(accessKey));
});

test("concurrent setup is serialized and failed storage checks can be retried", async (t) => {
  let release;
  let entered;
  const started = new Promise((resolve) => {
    entered = resolve;
  });
  const s = await fixture(t, {
    probe: async () => {
      entered();
      await new Promise((resolve) => {
        release = resolve;
      });
    },
  });
  const claim = s.claim();
  const pending = s.request("/setup/test", post(claim));
  await started;
  assert.equal((await s.request("/setup", post(claim))).status, 409);
  release();
  assert.equal((await pending).status, 200);
  assert.ok(!existsSync(join(s.directory, "server-config.json")));
});

test("S3 form validates and passes credentials only to authenticated probe", async (t) => {
  let config;
  const s = await fixture(t, {
    probe: async (value) => {
      config = value;
      throw new Error("Storage check failed");
    },
  });
  const input = {
    mode: "s3",
    endpoint: "https://s3.example.com",
    region: "region",
    bucket: "graft-test",
    accessKeyId: "access",
    secretAccessKey: "secret",
    pathStyle: false,
  };
  assert.equal((await s.request("/setup/test", post(s.claim(), input))).status, 400);
  assert.equal(config.forcePathStyle, false);
  assert.equal(config.credentials.secretAccessKey, "secret");
  assert.ok(!existsSync(join(s.directory, "server-config.json")));
  config = undefined;
  assert.equal(
    (
      await s.request(
        "/setup/test",
        post(s.claim(), { ...input, endpoint: "http://s3.example.com" }),
      )
    ).status,
    400,
  );
  assert.equal(config, undefined);
});

test("legacy key-file installations bypass setup and missing configuration fails closed", async (t) => {
  const directory = mkdtempSync(join(tmpdir(), "graft-legacy-"));
  t.after(() => rmSync(directory, { recursive: true }));
  const key = join(directory, "legacy-key");
  writeFileSync(key, randomBytes(32).toString("hex"));
  const service = await createService({ directory, origin, env: { GRAFT_ACCESS_KEY_FILE: key } });
  assert.equal(service.initialized, true);
  assert.equal((await service.request(new Request(origin + "/setup", post("wrong")))).status, 409);
  service.close();
  await assert.rejects(createService({ directory, origin, env: {} }), /Existing data/);
  assert.ok(!existsSync(join(directory, "setup-token")));
});
