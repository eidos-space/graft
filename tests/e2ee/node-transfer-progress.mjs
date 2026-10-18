// Real native SDK + TypeScript remote through a throttled loopback proxy.
// Run with GRAFT_SDK_NATIVE_PATH pointing at an E2EE-enabled addon.
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
import { DatabaseSync } from "node:sqlite";
import { Transform } from "node:stream";
import { setTimeout as delay } from "node:timers/promises";

const require = createRequire(import.meta.url);
const { EncryptedRemoteSession, RepositorySession } = require("../../packages/graft-sdk");
const directory = mkdtempSync(join(tmpdir(), "graft-encrypted-progress-"));
const uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
const reports = [];
const modes = process.env.GRAFT_PROGRESS_MODES?.split(",") ?? ["aggregate", "object", "multipart"];

function throttle() {
  return new Transform({
    transform(chunk, _encoding, callback) {
      setTimeout(() => callback(null, chunk), 12);
    },
  });
}

async function scenario(mode) {
  const fixture = join(directory, `${mode}.json`);
  const capture = join(directory, `${mode}-capture.json`);
  writeFileSync(fixture, JSON.stringify({
    encryption: { profile: "encrypted-remote-v1", version: 1, suite: 1, uuid },
    objects: [], serviceLifetimeMs: 180000,
    ...(mode === "multipart" ? { enableMultipart: true, maxRequestBytes: 1024 * 1024, multipartPartBytes: 512 * 1024 } : {}),
  }));
  const service = spawn(process.execPath, ["tests/e2ee/remote-service.mjs", fixture, capture], { stdio: ["ignore", "pipe", "inherit"] });
  const lines = createInterface({ input: service.stdout });
  const ready = JSON.parse((await once(lines, "line"))[0]);
  const upstream = new URL(ready.enrollmentUrl).origin;
  const proxy = createServer((incoming, outgoing) => {
    const upstreamRequest = request(new URL(incoming.url, upstream), {
      method: incoming.method, headers: { ...incoming.headers, host: new URL(upstream).host },
    }, async (response) => {
      const headers = { ...response.headers };
      // Exercise the mandatory individual-object fallback as well as bundles.
      if (mode !== "aggregate" && incoming.url === "/acme/enrollment") {
        const chunks = [];
        for await (const chunk of response) chunks.push(chunk);
        const descriptor = JSON.parse(Buffer.concat(chunks));
        descriptor.capabilities = descriptor.capabilities.filter((value) => !["e2ee-fetch-bundle", "e2ee-receive-bundle"].includes(value));
        const body = Buffer.from(JSON.stringify(descriptor));
        delete headers["transfer-encoding"];
        outgoing.writeHead(response.statusCode, { ...headers, "content-length": body.length });
        outgoing.end(body);
      } else {
        outgoing.writeHead(response.statusCode, headers);
        response.on("error", () => outgoing.destroy());
        response.pipe(throttle()).pipe(outgoing);
      }
    });
    upstreamRequest.on("error", () => outgoing.destroy());
    incoming.on("error", () => upstreamRequest.destroy());
    outgoing.on("close", () => upstreamRequest.destroy());
    incoming.pipe(throttle()).pipe(upstreamRequest);
  });
  proxy.listen(0, "127.0.0.1");
  await once(proxy, "listening");
  const url = `http://127.0.0.1:${proxy.address().port}/acme/enrollment`;
  const authorities = [];
  const repositories = [];
  const worktrees = new Map();
  function privateDirectory(name) {
    const path = join(directory, `${mode}-${name}`);
    EncryptedRemoteSession.createPrivateDirectory(path);
    return path;
  }
  function authority(name) {
    const value = new EncryptedRemoteSession({
      remoteName: "origin", url, uuid, authenticationDomain: "progress.test", accountSubject: "test-account", bearerToken: "account-token",
      checkpointDirectory: privateDirectory(`${name}-checkpoint`), createCheckpoint: true,
      recoveryDirectory: privateDirectory(`${name}-recovery`), stagingDirectory: privateDirectory(`${name}-staging`),
    });
    authorities.push(value);
    return value;
  }
  async function repository(name, encryptedRemote) {
    const root = privateDirectory(name);
    const value = await RepositorySession.open(root, encryptedRemote ? { encryptedRemote } : {});
    repositories.push(value);
    worktrees.set(value, root);
    return value;
  }
  async function observed(label, direction, operation) {
    const events = [];
    const allEvents = [];
    const lateEvents = [];
    const start = performance.now();
    let completed = false;
    const result = await operation((event) => {
      if (completed) lateEvents.push(event);
      allEvents.push(event);
      if (event.direction === direction) {
        events.push({ bytes: event.transferredBytes, ms: performance.now() - start });
      }
    });
    completed = true;
    const elapsedMs = performance.now() - start;
    await delay(30);
    assert.deepEqual(lateEvents, [], `${label}: events after settlement`);
    for (const event of allEvents) {
      assert.deepEqual(Object.keys(event).sort(), Object.keys(event).filter((key) => ["direction", "transferredBytes", "totalBytes", "totalIsFinal"].includes(key)).sort());
      assert.equal(event.totalIsFinal, false, `${label}: E2EE must not claim a planned total`);
      assert.equal(event.totalBytes, undefined);
    }
    for (let index = 1; index < events.length; index++) assert.ok(events[index].bytes >= events[index - 1].bytes);
    const positive = events.filter((event) => event.bytes > 0);
    assert.equal(events[0]?.bytes, 0, `${label}: counters must reset for each operation`);
    assert.ok(positive.length >= 3, `${mode}/${label}: missing intermediate progress ${JSON.stringify(events)}`);
    assert.ok(positive.some((event) => event.bytes < positive.at(-1).bytes && event.ms < elapsedMs - 30), `${label}: progress only at completion`);
    reports.push({ mode, label, elapsedMs: Math.round(elapsedMs), samples: events.length, bytes: positive.at(-1).bytes });
    return result;
  }
  try {
    const sourceAuthority = authority("source");
    const source = await repository("source", sourceAuthority);
    await source.init();
    const database = new DatabaseSync(join(worktrees.get(source), "records.sqlite"));
    database.exec("CREATE TABLE records(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO records VALUES(1, 'source');");
    database.close();
    const payload = randomBytes(8 * 1024 * 1024);
    writeFileSync(join(worktrees.get(source), "attachment.bin"), payload);
    await source.addAll();
    await source.commit("synthetic first version");
    const head = (await source.repositoryMetadata()).current_head;
    await sourceAuthority.initializeConfig(Buffer.from("synthetic transfer password"), { branch: "main", commit: head });
    await source.addEncryptedRemote();
    await observed("initial-upload", "upload", (onProgress) => source.pushInitialBranch({ onProgress, verificationDirectory: join(directory, `${mode}-verification`) }));
    const peerAuthority = authority("peer");
    await peerAuthority.unlock(Buffer.from("synthetic transfer password"));
    const peer = await repository("peer", peerAuthority);
    await observed("clone", "download", (onProgress) => peer.cloneEncryptedRemote({ onProgress }));
    assert.deepEqual(readFileSync(join(worktrees.get(peer), "attachment.bin")), payload);
    const peerDatabase = new DatabaseSync(join(worktrees.get(peer), "records.sqlite"));
    assert.equal(peerDatabase.prepare("SELECT value FROM records WHERE id = 1").get().value, "source");
    peerDatabase.exec("INSERT INTO records VALUES(2, 'peer');");
    peerDatabase.close();
    const update = randomBytes(8 * 1024 * 1024);
    writeFileSync(join(worktrees.get(peer), "update.bin"), update);
    await peer.addAll();
    await peer.commit("synthetic peer update");
    // Cancel while a body is transferring, then retry with a fresh reporter.
    const abort = new AbortController();
    let cancelledBytes = 0;
    await assert.rejects(peer.push({ signal: abort.signal, onProgress(event) {
      if (event.direction === "upload" && event.transferredBytes > 128 * 1024) {
        cancelledBytes = event.transferredBytes;
        abort.abort();
      }
    } }), { name: "AbortError" });
    assert.ok(cancelledBytes > 0, "cancellation did not reach a real upload");
    await observed("retry-upload", "upload", (onProgress) => peer.push({ onProgress }));
    const abortDownload = new AbortController();
    let cancelledDownloadBytes = 0;
    await assert.rejects(source.pull({ signal: abortDownload.signal, onProgress(event) {
      if (event.direction === "download" && event.transferredBytes > 128 * 1024) {
        cancelledDownloadBytes = event.transferredBytes;
        abortDownload.abort();
      }
    } }), { name: "AbortError" });
    assert.ok(cancelledDownloadBytes > 0, "cancellation did not reach a real download");
    await observed("pull", "download", (onProgress) => source.pull({ onProgress }));
    assert.deepEqual(readFileSync(join(worktrees.get(source), "update.bin")), update);
    const receivedDatabase = new DatabaseSync(join(worktrees.get(source), "records.sqlite"), { readOnly: true });
    assert.deepEqual(receivedDatabase.prepare("SELECT value FROM records ORDER BY id").all().map((row) => row.value), ["source", "peer"]);
    receivedDatabase.close();
    const requests = JSON.parse(readFileSync(capture));
    assert.ok(requests.some((request) => request.path.includes(mode === "aggregate" ? "/e2ee/receive-bundle/" : mode === "multipart" ? "/multipart-part/" : "/raw-if-not-exists/e2ee/files/")), `missing ${mode} transport`);
  } finally {
    for (const value of repositories.reverse()) await value.close();
    for (const value of authorities) await value.disconnect();
    proxy.closeAllConnections();
    await new Promise((resolve) => proxy.close(resolve));
    service.kill();
    await once(service, "exit");
    lines.close();
  }
}

try {
  for (const mode of modes) await scenario(mode);
  console.log(JSON.stringify({ status: "passed", reports }, null, 2));
} finally {
  rmSync(directory, { recursive: true, force: true });
}
