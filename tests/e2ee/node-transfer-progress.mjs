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
    const stages = [];
    const captureStart = JSON.parse(readFileSync(capture)).length;
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
    }, (event) => { assert.equal(completed, false, "sync progress after settlement"); stages.push(event); });
    completed = true;
    const elapsedMs = performance.now() - start;
    if (process.env.GRAFT_PROGRESS_SYNC !== "0") {
      assert.equal(stages[0]?.phase, "preparing");
      assert.equal(stages.at(-1)?.phase, "completed");
      const requests = new Map();
      for (const event of stages) if (event.transfer) {
        assert.ok(event.requestId > 0);
        const previous = requests.get(event.requestId);
        if (previous) assert.ok(event.transfer.transferredBytes >= previous.transferredBytes);
        requests.set(event.requestId, event.transfer);
        if (event.transfer.totalIsFinal) assert.ok(event.transfer.transferredBytes <= event.transfer.totalBytes);
      }
      assert.ok(requests.size > 0);
      if (label === "initial-upload") assert.ok(stages.some((e) => e.phase === "verifying" && e.transfer?.direction === "download"));
      if (["clone", "pull"].includes(label)) assert.ok(stages.some((e) => e.phase === "applying"));
    }
    await delay(30);
    assert.deepEqual(lateEvents, [], `${label}: events after settlement`);
    for (const event of allEvents) {
      assert.deepEqual(Object.keys(event).sort(), Object.keys(event).filter((key) => ["direction", "transferredBytes", "totalBytes", "totalIsFinal"].includes(key)).sort());
      if (event.totalIsFinal) {
        assert.equal(event.direction, "upload", "only complete upload inventories are implemented");
        assert.ok(event.totalBytes >= event.transferredBytes);
      }
    }
    for (let index = 1; index < events.length; index++) assert.ok(events[index].bytes >= events[index - 1].bytes);
    const positive = events.filter((event) => event.bytes > 0);
    assert.equal(events[0]?.bytes, 0, `${label}: counters must reset for each operation`);
    assert.ok(positive.length >= 3, `${mode}/${label}: missing intermediate progress ${JSON.stringify(events)}`);
    assert.ok(positive.some((event) => event.bytes < positive.at(-1).bytes && event.ms < elapsedMs - 30), `${label}: progress only at completion`);
    const planned = allEvents.filter((event) => event.direction === direction && event.totalIsFinal);
    if (mode === "multipart") assert.equal(planned.length, 0, "multipart must not declare an incomplete operation total");
    const plannedUpload = process.env.GRAFT_PROGRESS_SYNC !== "0" && mode !== "multipart" && ["initial-upload", "retry-upload"].includes(label);
    if (plannedUpload) {
      if (label === "initial-upload") {
        const sent = JSON.parse(readFileSync(capture)).slice(captureStart).filter((request) => request.method === "PUT" && /\/raw-if-not-exists\/e2ee\/(objects|packs|indexes|files|commits|segments)\//.test(request.path));
        assert.ok(sent.length > 1, "the plan must span multiple payload requests");
        assert.equal(sent.reduce((total, request) => total + request.body.length / 2, 0), planned.at(-1).totalBytes, "server body bytes differ from complete inventory");
      }
      assert.ok(planned.length >= 2, `${label}: no full upload inventory`);
      assert.equal(new Set(planned.map((event) => event.totalBytes)).size, 1, "denominator moved");
      assert.equal(planned.at(-1).transferredBytes, planned.at(-1).totalBytes);
      assert.ok(planned.some((event) => event.transferredBytes > 0 && event.transferredBytes < event.totalBytes), "missing intermediate planned bytes");
    }
    reports.push({ mode, label, plannedTotal: planned.at(-1)?.totalBytes, elapsedMs: Math.round(elapsedMs), samples: events.length, bytes: positive.at(-1).bytes });
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
    await observed("initial-upload", "upload", (onProgress, onSyncProgress) => source.pushInitialBranch({ onProgress, onSyncProgress: process.env.GRAFT_PROGRESS_SYNC === "0" ? undefined : onSyncProgress, verificationDirectory: join(directory, `${mode}-verification`) }));
    const peerAuthority = authority("peer");
    await peerAuthority.unlock(Buffer.from("synthetic transfer password"));
    const peer = await repository("peer", peerAuthority);
    await observed("clone", "download", (onProgress, onSyncProgress) => peer.cloneEncryptedRemote({ onProgress, onSyncProgress: process.env.GRAFT_PROGRESS_SYNC === "0" ? undefined : onSyncProgress }));
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
    const cancelledStages = [];
    await assert.rejects(peer.push({ signal: abort.signal, onSyncProgress: process.env.GRAFT_PROGRESS_SYNC === "0" ? undefined : (event) => cancelledStages.push(event), onProgress(event) {
      if (event.direction === "upload" && event.transferredBytes > 128 * 1024) {
        cancelledBytes = event.transferredBytes;
        abort.abort();
      }
    } }), { name: "AbortError" });
    if (process.env.GRAFT_PROGRESS_SYNC !== "0") assert.equal(cancelledStages.at(-1)?.phase, "cancelled");
    const settledCount = cancelledStages.length;
    await delay(50);
    assert.equal(cancelledStages.length, settledCount, "sync event after cancellation settlement");
    assert.ok(cancelledBytes > 0, "cancellation did not reach a real upload");
    await observed("retry-upload", "upload", (onProgress, onSyncProgress) => peer.push({ onProgress, onSyncProgress: process.env.GRAFT_PROGRESS_SYNC === "0" ? undefined : onSyncProgress }));
    const abortDownload = new AbortController();
    let cancelledDownloadBytes = 0;
    const cancelledDownloads = [];
    await assert.rejects(source.pull({ signal: abortDownload.signal, onSyncProgress: process.env.GRAFT_PROGRESS_SYNC === "0" ? undefined : (event) => cancelledDownloads.push(event), onProgress(event) {
      if (event.direction === "download" && event.transferredBytes > 128 * 1024) {
        cancelledDownloadBytes = event.transferredBytes;
        abortDownload.abort();
      }
    } }), { name: "AbortError" });
    if (process.env.GRAFT_PROGRESS_SYNC !== "0") assert.equal(cancelledDownloads.at(-1)?.phase, "cancelled");
    assert.ok(cancelledDownloadBytes > 0, "cancellation did not reach a real download");
    await observed("pull", "download", (onProgress, onSyncProgress) => source.pull({ onProgress, onSyncProgress: process.env.GRAFT_PROGRESS_SYNC === "0" ? undefined : onSyncProgress }));
    assert.deepEqual(readFileSync(join(worktrees.get(source), "update.bin")), update);
    const receivedDatabase = new DatabaseSync(join(worktrees.get(source), "records.sqlite"), { readOnly: true });
    assert.deepEqual(receivedDatabase.prepare("SELECT value FROM records ORDER BY id").all().map((row) => row.value), ["source", "peer"]);
    receivedDatabase.close();
    const requests = JSON.parse(readFileSync(capture));
    assert.ok(requests.some((request) => request.path.includes(mode === "aggregate" && process.env.GRAFT_PROGRESS_SYNC === "0" ? "/e2ee/receive-bundle/" : mode === "multipart" ? "/multipart-part/" : "/raw-if-not-exists/e2ee/files/")), `missing ${mode} transport`);
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
