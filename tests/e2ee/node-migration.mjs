// Actual native migration/cutover and TypeScript service; synthetic data only.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
import { once } from "node:events";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
const require = createRequire(import.meta.url);
const { EncryptedRemoteSession, RepositorySession } = require("../../packages/graft-sdk");
const [fixture, directory] = process.argv.slice(2);
const capture = join(directory, "captures.json");
const server = spawn("node", ["tests/e2ee/remote-service.mjs", fixture, capture], { stdio: ["ignore", "pipe", "inherit"] });
const lines = createInterface({ input: server.stdout });
const timeout = setTimeout(() => { server.kill(); throw new Error("Node migration timed out"); }, 180000);
let authority;
let source;
try {
  const [line] = await once(lines, "line");
  const ready = JSON.parse(line);
  function privateDirectory(name) { const path = join(directory, name); mkdirSync(path, { mode: 0o700 }); return path; }
  const options = { remoteName: "vault", url: ready.migrationUrl,
    uuid: "40010203-0405-4607-8809-0a0b0c0d0e0f", authenticationDomain: "native.test", accountSubject: "test-account", bearerToken: "account-token",
    checkpointDirectory: privateDirectory("checkpoint"), recoveryDirectory: privateDirectory("recovery"), stagingDirectory: privateDirectory("staging"), createCheckpoint: true };
  const password = () => Buffer.from("Node migration 密码 raw \r\n");
  let lastCapture = [];
  const captured = () => {
    if (!existsSync(capture)) return lastCapture;
    try { lastCapture = JSON.parse(readFileSync(capture)); }
    catch (error) { if (!(error instanceof SyntaxError)) throw error; }
    return lastCapture;
  };
  authority = new EncryptedRemoteSession(options);
  source = await RepositorySession.open(join(directory, "source"), { encryptedRemote: authority });
  const originalConfig = readFileSync(join(directory, "source/.graft/config.toml"));
  const preparation = { allRefs: true, mappings: [
    { source: "refs/heads/main", target: "refs/heads/moved" },
    { source: "refs/heads/older", target: "refs/heads/old-history" },
    { source: "refs/heads/empty", target: "refs/heads/future" },
    { source: "refs/tags/release", target: "refs/tags/retained" },
  ], targetHead: "refs/heads/moved" };
  const invalid = password();
  await assert.rejects(source.prepareEncryptedMigration(invalid, { ...preparation, mappings: preparation.mappings.slice(0, 1) }));
  assert.ok(invalid.every((byte) => byte === 0));
  assert.equal(authority.state, "locked");
  assert.ok(!existsSync(join(options.recoveryDirectory, "migration-source")));
  assert.equal(captured().length, 0);
  const input = password();
  const preparing = source.prepareEncryptedMigration(input, preparation);
  assert.ok(input.every((byte) => byte === 0));
  assert.equal(authority.state, "unlocking");
  assert.equal((await preparing).status, "prepared");
  console.log("Node migration prepared offline");
  assert.equal(authority.state, "locked");
  assert.equal(captured().length, 0);
  await source.close();
  await authority.disconnect();
  authority = new EncryptedRemoteSession({ ...options, createCheckpoint: false });
  source = await RepositorySession.open(join(directory, "source"), { encryptedRemote: authority });
  await assert.rejects(source.resumeEncryptedMigration(password()), { code: "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN" });
  assert.equal(authority.state, "locked");
  await assert.rejects(source.resumeEncryptedMigration(password()), { code: "GRAFT_SDK_REMOTE_PUBLICATION_OUTCOME_UNKNOWN" });
  await authority.unlock(password());
  const uploads = authority.retainedUploads();
  assert.equal(uploads.length, 1);
  assert.ok(uploads[0].serverSessionRecorded);
  // Lost completion response is resolved by authenticating the complete winner.
  assert.equal(await authority.restartRetainedUpload(uploads[0].opaqueKey), "reused");
  assert.deepEqual(authority.retainedUploads(), []);
  assert.ok(captured().some((entry) => entry.path.includes("/multipart-abort/")));
  await authority.lock();
  assert.equal((await source.resumeEncryptedMigration(password())).status, "ready-for-independent-verification");
  console.log("Node migration publication and retained multipart restart confirmed");
  const initialCreates = captured().filter((entry) => entry.method === "PUT" && entry.path === "/acme/migration/raw-if-not-exists/e2ee/config");
  assert.ok(initialCreates.length >= 1);
  assert.equal(new Set(initialCreates.map((entry) => entry.body)).size, 1);
  assert.deepEqual(readFileSync(join(directory, "source/.graft/config.toml")), originalConfig);
  const occupied = privateDirectory("occupied");
  writeFileSync(join(occupied, "sentinel"), "keep");
  const beforeReject = captured().length;
  const occupiedPassword = password();
  await assert.rejects(source.cutoverEncryptedMigration(occupiedPassword, { verificationDirectory: occupied, bearerToken: "account-token" }));
  assert.ok(occupiedPassword.every((byte) => byte === 0));
  assert.equal(readFileSync(join(occupied, "sentinel"), "utf8"), "keep");
  assert.equal(captured().length, beforeReject);
  const verifyOptions = { verificationDirectory: join(directory, "verification"), bearerToken: "account-token" };
  await assert.rejects(source.verifyEncryptedMigration(Buffer.from("wrong password"), verifyOptions), { code: "GRAFT_SDK_E2EE_UNLOCK_FAILED" });
  assert.ok(!existsSync(verifyOptions.verificationDirectory));
  const verified = await source.verifyEncryptedMigration(password(), verifyOptions);
  assert.equal(verified.status, "independently-verified");
  assert.equal(verified.references, 4);
  console.log("Node migration all-ref independent recovery confirmed");
  assert.deepEqual(readFileSync(join(directory, "source/.graft/config.toml")), originalConfig);
  const beforeCutover = captured().length;
  const cutover = source.cutoverEncryptedMigration(password(), { ...verifyOptions, verificationDirectory: join(directory, "cutover-verification") });
  let cutoverSettled = false;
  const observedCutover = cutover.finally(() => { cutoverSettled = true; });
  while (!cutoverSettled && captured().length === beforeCutover) await delay(10);
  assert.equal(cutoverSettled, false);
  await assert.rejects(source.addAll(), { code: "GRAFT_SDK_REPOSITORY_BUSY" });
  const cutoverResult = await observedCutover;
  assert.equal(cutoverResult.status, "host-cutover-confirmed");
  assert.equal(cutoverResult.offlineDevices, "pending");
  console.log("Node migration native fenced local cutover confirmed");
  assert.equal(authority.state, "locked");
  const config = readFileSync(join(directory, "source/.graft/config.toml"), "utf8");
  assert.ok(config.includes(ready.migrationUrl));
  assert.ok(config.includes('merge = "refs/heads/moved"'));
  assert.ok(!config.includes("account-token"));
  assert.equal(readFileSync(join(directory, "source/attachment.txt"), "utf8"), "Node migration private file");
  assert.equal((await source.cutoverEncryptedMigration(password(), { ...verifyOptions, verificationDirectory: join(directory, "cutover-replay") })).hostCutover, "confirmed");
  await authority.unlock(password());
  assert.deepEqual(authority.retainedUploads(), []);
  const beforeMissing = captured().length;
  await assert.rejects(authority.restartRetainedUpload(`e2ee/objects/${"00".repeat(32)}`));
  assert.ok(captured().slice(beforeMissing).every((entry) => entry.method === "GET" || entry.method === "HEAD"));
  const wire = JSON.stringify(captured());
  for (const privateValue of ["Node migration 密码", "attachment.txt", "large-private.bin", "first private migration commit", "refs/heads/old-history", "Node migration first WAL row"]) {
    assert.ok(!wire.includes(privateValue), `private input appeared in remote traffic: ${privateValue}`);
    assert.ok(!wire.includes(Buffer.from(privateValue).toString("hex")), `private input appeared in captured binary body: ${privateValue}`);
  }
  console.log("Native Node migration: 4 refs, retained resume, fresh recovery, native fenced cutover and replay passed");
} finally {
  if (source) await source.close();
  if (authority) await authority.disconnect();
  clearTimeout(timeout);
  lines.close();
  if (server.exitCode === null && server.signalCode === null) {
    const exited = once(server, "exit");
    server.kill();
    await exited;
  }
}
