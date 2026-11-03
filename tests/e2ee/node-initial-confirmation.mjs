// Fault injection against the real native SDK and actual Remote v1 service.
// All repositories, credentials and payloads are synthetic and temporary.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { once } from "node:events";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { createServer, request } from "node:http";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
const { EncryptedRemoteSession, RepositorySession } = createRequire(import.meta.url)("../../packages/graft-sdk");
const root = mkdtempSync(join(tmpdir(), "graft-initial-confirmation-"));
const uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
const password = "synthetic confirmation password";
const results = [];

async function scenario(name) {
  const fixture = join(root, `${name}.json`);
  writeFileSync(fixture, JSON.stringify({ encryption: { profile: "encrypted-remote-v1", version: 1, suite: 1, uuid }, objects: [], serviceLifetimeMs: 180000 }));
  const service = spawn(process.execPath, ["tests/e2ee/remote-service.mjs", fixture, join(root, `${name}-requests.json`)], { stdio: ["ignore", "pipe", "inherit"] });
  const lines = createInterface({ input: service.stdout });
  const ready = JSON.parse((await once(lines, "line"))[0]);
  let fault = name;
  let headPublished = false;
  let injected = 0;
  let crashChild;
  let cancelUpload;
  const requests = [];
  const proxy = createServer((incoming, outgoing) => {
    const path = incoming.url;
    requests.push({ method: incoming.method, path });
    const payload = /e2ee\/(files|objects|packs|commits|segments|indexes)\//.test(path);
    if (fault === "cancel-upload" && incoming.method === "PUT" && payload) {
      incoming.once("data", () => { injected++; cancelUpload(); });
    }
    if (fault === "upload-failure" && incoming.method === "PUT" && payload) {
      injected++;
      incoming.resume();
      outgoing.writeHead(503, { "Graft-Protocol": "1" }); outgoing.end(); return;
    }
    if (fault === "lost-ref-response" && injected > 0 && incoming.method === "GET" && path.includes("/e2ee/refs/")) {
      outgoing.writeHead(503, { "Graft-Protocol": "1" }); outgoing.end(); return;
    }
    if (["missing-payload", "corrupt-payload"].includes(fault) && incoming.method === "GET" && payload) {
      injected++;
      outgoing.writeHead(fault === "missing-payload" ? 404 : 200, { "Graft-Protocol": "1" });
      outgoing.end(fault === "missing-payload" ? undefined : Buffer.alloc(128)); return;
    }
    const upstream = request(new URL(path, new URL(ready.enrollmentUrl).origin), { method: incoming.method, headers: { ...incoming.headers, host: new URL(ready.enrollmentUrl).host } }, async (response) => {
      const chunks = [];
      for await (const chunk of response) chunks.push(chunk);
      let body = Buffer.concat(chunks);
      let status = response.statusCode;
      const headers = { ...response.headers };
      delete headers["transfer-encoding"];
      if (incoming.method === "POST" && path.includes("/e2ee/head/") && status === 204) headPublished = true;
      if (incoming.method === "GET" && path === "/acme/enrollment") {
        const descriptor = JSON.parse(body);
        // Use object publication so individual upload failures are observable.
        descriptor.capabilities = descriptor.capabilities.filter((cap) => !["e2ee-fetch-bundle", "e2ee-receive-bundle"].includes(cap));
        if (headPublished && fault === "wrong-identity") { descriptor.encryption.uuid = "10010203-0405-4607-8809-0a0b0c0d0e0f"; injected++; }
        body = Buffer.from(JSON.stringify(descriptor));
      }
      if (headPublished && incoming.method === "GET") {
        if (fault === "repository-deleted") { status = 404; body = Buffer.alloc(0); injected++; }
        if (fault === "process-exit" && path === "/acme/enrollment") {
          injected++; crashChild.kill("SIGKILL"); outgoing.destroy(); return;
        }
        if (fault === "config-mismatch" && path.endsWith("/e2ee/config")) { body[body.length - 1] ^= 1; injected++; }
        if (fault === "missing-head" && path.includes("/raw/e2ee/head/")) { status = 404; body = Buffer.alloc(0); injected++; }
        if (fault === "corrupt-ref" && path.includes("/raw/e2ee/refs/")) { body[body.length - 1] ^= 1; injected++; }
        if (fault === "confirmation-offline" && path === "/acme/enrollment") { status = 503; body = Buffer.alloc(0); injected++; }
      }
      if (fault === "lost-object-response" && incoming.method === "PUT" && payload && status === 204 && injected === 0) {
        injected++; outgoing.destroy(); return;
      }
      if (fault === "lost-ref-response" && incoming.method === "POST" && path.includes("/e2ee/refs/") && status === 204 && injected === 0) {
        injected++; outgoing.destroy(); return;
      }
      headers["content-length"] = body.length;
      outgoing.writeHead(status, headers); outgoing.end(body);
    });
    upstream.on("error", () => outgoing.destroy());
    incoming.pipe(upstream);
  });
  proxy.listen(0, "127.0.0.1"); await once(proxy, "listening");
  const privateDirectory = (label) => { const path = join(root, `${name}-${label}`); EncryptedRemoteSession.createPrivateDirectory(path); return path; };
  const options = {
    remoteName: "origin", url: `http://127.0.0.1:${proxy.address().port}/acme/enrollment`, uuid,
    authenticationDomain: "confirmation.test", accountSubject: "synthetic-user", bearerToken: "account-token",
    checkpointDirectory: privateDirectory("checkpoint"), createCheckpoint: true,
    recoveryDirectory: privateDirectory("recovery"), stagingDirectory: privateDirectory("staging"),
  };
  let authority = new EncryptedRemoteSession(options);
  const sourcePath = privateDirectory("source");
  let source = await RepositorySession.open(sourcePath, { encryptedRemote: authority });
  try {
    await source.init();
    const content = randomBytes(2 * 1024 * 1024);
    writeFileSync(join(sourcePath, "payload.bin"), content);
    await source.addAll(); await source.commit("initial");
    const head = (await source.repositoryMetadata()).current_head;
    assert.equal(await authority.initializeConfig(Buffer.from(password), { branch: "main", commit: head }), "config_published");
    await source.addEncryptedRemote();
    const phases = [];
    const abort = new AbortController();
    cancelUpload = () => abort.abort();
    let failed = false;
    let failureCode;
    try {
      if (name === "process-exit") {
        await source.close(); await authority.disconnect();
        crashChild = spawn(process.execPath, ["-e", `
          const { EncryptedRemoteSession, RepositorySession } = require('./packages/graft-sdk');
          (async () => {
            const authority = new EncryptedRemoteSession(JSON.parse(process.argv[1]));
            await authority.unlock(Buffer.from(process.argv[3]));
            const source = await RepositorySession.open(process.argv[2], { encryptedRemote: authority });
            await source.pushInitialBranch();
            process.exit(0);
          })().catch(error => { console.error(error); process.exit(1); });
        `, JSON.stringify({ ...options, createCheckpoint: false }), sourcePath, password], { stdio: ["ignore", "ignore", "inherit"] });
        const [code, signal] = await once(crashChild, "exit");
        assert.equal(code, null); assert.equal(signal, "SIGKILL");
        failed = true;
      } else {
        await source.pushInitialBranch({ signal: abort.signal, onSyncProgress: (event) => phases.push(event.phase), onProgress(event) {
          if (fault === "cancel-upload" && event.direction === "upload" && event.transferredBytes > 65536) { injected++; abort.abort(); }
        } });
      }
    } catch (error) { failed = true; failureCode = error.code ?? error.name; }
    if (name === "wrong-identity") assert.equal(failureCode, "GRAFT_SDK_E2EE_IDENTITY_CHANGED");
    if (name === "repository-deleted") assert.equal(failureCode, "GRAFT_SDK_E2EE_HTTP_STATUS");
    if (name !== "lost-object-response") {
      assert.equal(failed, true, `${name}: failure accepted as success`);
      assert.ok(!phases.includes("completed"), `${name}: emitted completion on failure`);
      assert.notEqual(readFileSync(join(options.recoveryDirectory, "initialization"))[40], 4);
    }
    assert.ok(injected > 0, `${name}: injection did not execute`);
    if (name === "upload-failure") assert.equal(headPublished, false, "published HEAD despite missing dependency");
    await source.close(); await authority.disconnect();
    fault = null;
    authority = new EncryptedRemoteSession({ ...options, createCheckpoint: false });
    await authority.unlock(Buffer.from(password));
    source = await RepositorySession.open(sourcePath, { encryptedRemote: authority });
    const retry = await source.pushInitialBranch();
    assert.equal(retry.state, "connected"); assert.equal(retry.verification, "publication");
    assert.equal(readFileSync(join(options.recoveryDirectory, "initialization"))[40], 4);
    assert.equal((await source.repositoryMetadata()).current_head, head);
    const repeat = await source.pushInitialBranch(); assert.equal(repeat.push.commits, 0);
    // A publication receipt must never masquerade as full recovery evidence.
    for (const corrupt of ["missing-payload", "corrupt-payload"]) {
      fault = corrupt;
      await assert.rejects(source.pushInitialBranch({ verificationDirectory: join(root, `${name}-${corrupt}-restore`) }));
    }
    fault = null;
    const restored = join(root, `${name}-restore`);
    assert.equal((await source.pushInitialBranch({ verificationDirectory: restored })).verification, "full_restore");
    assert.deepEqual(readFileSync(join(restored, "payload.bin")), content);
    // Changed local HEAD cannot replace the durable initial intent.
    writeFileSync(join(sourcePath, "later.txt"), "later"); await source.addAll(); await source.commit("later");
    const before = requests.length;
    await assert.rejects(source.pushInitialBranch(), { code: "GRAFT_SDK_E2EE_STALE_OBSERVATION" });
    assert.equal(requests.length, before);
    results.push({ name, injected, failureCode, recovered: true });
  } finally {
    await source.close(); await authority.disconnect();
    proxy.closeAllConnections(); await new Promise((resolve) => proxy.close(resolve));
    service.kill(); await once(service, "exit"); lines.close();
  }
}
try {
  for (const name of ["upload-failure", "lost-object-response", "lost-ref-response", "cancel-upload", "process-exit", "wrong-identity", "config-mismatch", "missing-head", "corrupt-ref", "confirmation-offline", "repository-deleted"]) await scenario(name);
  console.log(JSON.stringify(results, null, 2));
} finally { rmSync(root, { recursive: true, force: true }); }
