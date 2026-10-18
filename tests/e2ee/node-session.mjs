// Real native Node addon + actual TypeScript remote. All values are synthetic.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { chmodSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { createInterface } from "node:readline";
import { once } from "node:events";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { assertLegacyRequestPrivacy } from "./privacy-evidence.mjs";

const require = createRequire(import.meta.url);
const { EncryptedRemoteSession, RepositorySession } = require("../../packages/graft-sdk");
const [fixture, directory] = process.argv.slice(2);
const capture = join(directory, "captures.json");
const service = spawn("node", ["tests/e2ee/remote-service.mjs", fixture, capture], { stdio: ["ignore", "pipe", "inherit"] });
const lines = createInterface({ input: service.stdout });
const timeout = setTimeout(() => { service.kill(); throw new Error("Node E2EE scenario timed out"); }, 180000);
let db;
let source;
let peer;
let authority;
let peerAuthority;
let enrollmentAuthority;
let losingEnrollment;
let initialSource;
let wrongInitialSource;
let namedPeer;
try {
  const [line] = await once(lines, "line");
  const ready = JSON.parse(line);
  const uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
  function privateDirectory(name) { const path = join(directory, name); EncryptedRemoteSession.createPrivateDirectory(path); return path; }
  function options(name, extra = {}) { return { remoteName: "origin", url: ready.integrationUrl, uuid, authenticationDomain: "native.test", accountSubject: "test-account", bearerToken: "account-token", checkpointDirectory: privateDirectory(`${name}-checkpoint`), createCheckpoint: true, stagingDirectory: privateDirectory(`${name}-staging`), recoveryDirectory: privateDirectory(`${name}-recovery`), ...extra }; }
  const enrollmentOptions = options("enrollment", { url: ready.enrollmentUrl, remoteName: "vault" });
  enrollmentAuthority = new EncryptedRemoteSession(enrollmentOptions);
  const invalidInitial = Buffer.from([0xff]);
  await assert.rejects(enrollmentAuthority.initializeConfig(invalidInitial), { code: "GRAFT_SDK_E2EE_INVALID_PASSWORD" });
  assert.ok(invalidInitial.every((value) => value === 0));
  const initialSourceDirectory = privateDirectory("initial-source");
  initialSource = new RepositorySession(initialSourceDirectory, { encryptedRemote: enrollmentAuthority });
  await initialSource.open();
  await initialSource.init();
  await initialSource.addEncryptedRemote();
  writeFileSync(join(initialSourceDirectory, "initial-node-private.txt"), "node initial private attachment");
  await initialSource.addAll();
  await initialSource.commit("node initial private commit");
  const initialHead = (await initialSource.repositoryMetadata()).current_head;
  const initialPassword = Buffer.from("node random enrollment password");
  const initializing = enrollmentAuthority.initializeConfig(initialPassword, { branch: "main", commit: initialHead });
  assert.ok(initialPassword.every((value) => value === 0));
  assert.equal(enrollmentAuthority.state, "unlocking");
  assert.equal(await initializing, "unknown");
  assert.equal(enrollmentAuthority.state, "unlocked");
  const initialPut = JSON.parse(readFileSync(capture)).find((request) => request.path === "/acme/enrollment/raw-if-not-exists/e2ee/config");
  assert.ok(initialPut.droppedInitialConfigResponse);
  await initialSource.close();
  await enrollmentAuthority.disconnect();
  enrollmentAuthority = new EncryptedRemoteSession({ ...enrollmentOptions, createCheckpoint: false });
  const wrongInitial = Buffer.from("wrong node enrollment password");
  await assert.rejects(enrollmentAuthority.resumeInitialConfig(wrongInitial), { code: "GRAFT_SDK_E2EE_UNLOCK_FAILED" });
  assert.ok(wrongInitial.every((value) => value === 0));
  const initialResume = Buffer.from("node random enrollment password");
  const beforeInitialResume = JSON.parse(readFileSync(capture)).length;
  const resumingInitial = enrollmentAuthority.resumeInitialConfig(initialResume);
  assert.ok(initialResume.every((value) => value === 0));
  assert.equal(await resumingInitial, "config_published");
  assert.ok(JSON.parse(readFileSync(capture)).slice(beforeInitialResume).every((request) => request.method === "GET"));
  await enrollmentAuthority.lock();
  const recreateInitial = Buffer.from("node replacement enrollment password");
  await assert.rejects(enrollmentAuthority.initializeConfig(recreateInitial), { code: "GRAFT_SDK_E2EE_STALE_OBSERVATION" });
  assert.ok(recreateInitial.every((value) => value === 0));
  losingEnrollment = new EncryptedRemoteSession(options("losing-enrollment", { url: ready.enrollmentUrl }));
  assert.equal(await losingEnrollment.initializeConfig(Buffer.from("node competing enrollment password")), "conflict");
  assert.equal(losingEnrollment.state, "locked");
  const initialPuts = JSON.parse(readFileSync(capture)).filter((request) => request.path === "/acme/enrollment/raw-if-not-exists/e2ee/config");
  assert.equal(initialPuts.length, 2);
  assert.notEqual(initialPuts[0].body, initialPuts[1].body);
  await enrollmentAuthority.unlock(Buffer.from("node random enrollment password"));
  initialSource = new RepositorySession(initialSourceDirectory, { encryptedRemote: enrollmentAuthority });
  await initialSource.open();
  // Reject unsafe verifiers before any source publication or journal mutation.
  function snapshotFiles(path) {
    return readdirSync(path, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name)).map((entry) => [entry.name, entry.isDirectory() ? snapshotFiles(join(path, entry.name)) : readFileSync(join(path, entry.name)).toString("hex")]);
  }
  const occupiedVerifier = privateDirectory("occupied-verifier");
  writeFileSync(join(occupiedVerifier, "initial-node-private.txt"), "irreplaceable existing content");
  for (const [verificationDirectory, code] of [
    [occupiedVerifier, "GRAFT_SDK_REPOSITORY_COMMAND"],
    [initialSourceDirectory, "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING"],
    [join(initialSourceDirectory, "nested-verifier"), "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING"],
    [enrollmentOptions.checkpointDirectory, "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING"],
    [enrollmentOptions.stagingDirectory, "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING"],
    [enrollmentOptions.recoveryDirectory, "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING"],
    [join(enrollmentOptions.stagingDirectory, "nested-verifier"), "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING"],
  ]) {
    const requestsBefore = readFileSync(capture, "utf8");
    const recoveryBefore = snapshotFiles(enrollmentOptions.recoveryDirectory);
    const occupiedBefore = snapshotFiles(occupiedVerifier);
    const existedBefore = existsSync(verificationDirectory);
    await assert.rejects(initialSource.pushInitialBranch({ verificationDirectory }), { code });
    assert.equal(readFileSync(capture, "utf8"), requestsBefore);
    assert.deepEqual(snapshotFiles(enrollmentOptions.recoveryDirectory), recoveryBefore);
    assert.deepEqual(snapshotFiles(occupiedVerifier), occupiedBefore);
    assert.equal(existsSync(verificationDirectory), existedBefore);
    assert.equal((await initialSource.repositoryMetadata()).current_head, initialHead);
  }
  await assert.rejects(initialSource.pushInitialBranch(), { code: "GRAFT_SDK_REMOTE_PUBLICATION_UNCONFIRMED" });
  await initialSource.close();
  await enrollmentAuthority.disconnect();
  enrollmentAuthority = new EncryptedRemoteSession({ ...enrollmentOptions, createCheckpoint: false });
  await enrollmentAuthority.unlock(Buffer.from("node random enrollment password"));
  const wrongInitialDirectory = privateDirectory("wrong-initial-source");
  wrongInitialSource = new RepositorySession(wrongInitialDirectory, { encryptedRemote: enrollmentAuthority });
  await wrongInitialSource.open();
  await wrongInitialSource.init();
  await wrongInitialSource.addEncryptedRemote();
  writeFileSync(join(wrongInitialDirectory, "different.txt"), "different source");
  await wrongInitialSource.addAll();
  await wrongInitialSource.commit("different source commit");
  const beforeWrongInitial = JSON.parse(readFileSync(capture)).length;
  await assert.rejects(wrongInitialSource.pushInitialBranch(), { code: "GRAFT_SDK_E2EE_STALE_OBSERVATION" });
  assert.equal(JSON.parse(readFileSync(capture)).length, beforeWrongInitial);
  await wrongInitialSource.close();
  initialSource = new RepositorySession(initialSourceDirectory, { encryptedRemote: enrollmentAuthority });
  await initialSource.open();
  const connectedDirectory = join(directory, "initial-connected-verifier");
  const connected = await initialSource.pushInitialBranch({ verificationDirectory: connectedDirectory });
  assert.equal(connected.state, "connected");
  assert.equal(connected.push.commits, 0);
  assert.equal(readFileSync(join(connectedDirectory, "initial-node-private.txt"), "utf8"), "node initial private attachment");
  assert.equal((await initialSource.repositoryMetadata()).current_head, initialHead);
  const namedPeerDirectory = privateDirectory("named-peer");
  namedPeer = await RepositorySession.open(namedPeerDirectory, { encryptedRemote: enrollmentAuthority });
  await namedPeer.cloneEncryptedRemote({ branch: "main" });
  assert.equal((await namedPeer.repositoryMetadata()).current_head, initialHead);
  assert.equal((await namedPeer.listRemotes()).remotes[0].name, "vault");
  assert.equal(readFileSync(join(namedPeerDirectory, "initial-node-private.txt"), "utf8"), "node initial private attachment");
  await enrollmentAuthority.lock();
  const beforeNamedLocked = readFileSync(capture, "utf8");
  await assert.rejects(namedPeer.fetch({ remote: "vault", branch: "main" }), { code: "GRAFT_SDK_E2EE_LOCKED" });
  assert.equal(readFileSync(capture, "utf8"), beforeNamedLocked);
  await enrollmentAuthority.unlock(Buffer.from("node random enrollment password"));
  enrollmentAuthority.refreshBearerToken("native.test", "test-account", "incorrect-account-token");
  const beforeNamedUnauthorized = JSON.parse(readFileSync(capture)).length;
  await assert.rejects(namedPeer.fetch({ remote: "vault", branch: "main" }), { code: "GRAFT_SDK_E2EE_AUTHENTICATION_FAILED" });
  const namedUnauthorizedRequests = JSON.parse(readFileSync(capture)).slice(beforeNamedUnauthorized);
  assert.equal(namedUnauthorizedRequests.length, 1);
  assert.equal(namedUnauthorizedRequests[0].authenticated, false);
  enrollmentAuthority.refreshBearerToken("native.test", "test-account", "renewed-account-token");
  await namedPeer.fetch({ remote: "vault", branch: "main" });
  await enrollmentAuthority.disconnect();
  const beforeNamedDisconnected = readFileSync(capture, "utf8");
  await assert.rejects(namedPeer.fetch({ remote: "vault", branch: "main" }), { code: "GRAFT_SDK_E2EE_LOCKED" });
  assert.equal(readFileSync(capture, "utf8"), beforeNamedDisconnected);
  enrollmentAuthority.reconnect("account-token");
  await enrollmentAuthority.unlock(Buffer.from("node random enrollment password"));
  await namedPeer.fetch({ remote: "vault", branch: "main" });
  const firstOptions = options("source");
  const unsafeOptions = options("unsafe-recovery");
  if (process.platform === "win32") execFileSync("icacls", [unsafeOptions.recoveryDirectory, "/grant", "*S-1-1-0:(OI)(CI)F"]);
  else chmodSync(unsafeOptions.recoveryDirectory, 0o755);
  assert.throws(() => new EncryptedRemoteSession(unsafeOptions), { code: "GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE" });
  assert.equal(existsSync(join(unsafeOptions.checkpointDirectory, "checkpoint")), false);
  const disposableOptions = options("disposable-recovery");
  assert.throws(() => new EncryptedRemoteSession({ ...disposableOptions, recoveryDirectory: disposableOptions.stagingDirectory }), { code: "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING" });
  assert.equal(existsSync(join(disposableOptions.checkpointDirectory, "checkpoint")), false);
  const lostOptions = options("lost-recovery");
  const lostAuthority = new EncryptedRemoteSession(lostOptions);
  await lostAuthority.disconnect();
  rmSync(lostOptions.recoveryDirectory, { recursive: true });
  EncryptedRemoteSession.createPrivateDirectory(lostOptions.recoveryDirectory);
  const beforeLostReopen = existsSync(capture) ? readFileSync(capture, "utf8") : null;
  assert.throws(() => new EncryptedRemoteSession({ ...lostOptions, createCheckpoint: false }), { code: "GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE" });
  // Failed enrollment cannot manufacture a replacement namespace next to an
  // existing checkpoint, enabling a subsequent reopen with lost candidates.
  assert.throws(() => new EncryptedRemoteSession(lostOptions), { code: "GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE" });
  assert.throws(() => new EncryptedRemoteSession({ ...lostOptions, createCheckpoint: false }), { code: "GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE" });
  assert.equal(existsSync(join(lostOptions.recoveryDirectory, "ref-allocations")), false);
  assert.equal(existsSync(capture) ? readFileSync(capture, "utf8") : null, beforeLostReopen);
  const lostMultipartOptions = options("lost-multipart-recovery");
  const lostMultipartAuthority = new EncryptedRemoteSession(lostMultipartOptions);
  await lostMultipartAuthority.disconnect();
  const multipartDirectory = join(lostMultipartOptions.recoveryDirectory, "multipart");
  assert.equal(existsSync(join(multipartDirectory, "ref-allocations")), true);
  rmSync(multipartDirectory, { recursive: true });
  const beforeMultipartReopen = snapshotFiles(lostMultipartOptions.recoveryDirectory);
  assert.throws(() => new EncryptedRemoteSession({ ...lostMultipartOptions, createCheckpoint: false }), { code: "GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE" });
  assert.throws(() => new EncryptedRemoteSession(lostMultipartOptions), { code: "GRAFT_SDK_E2EE_CHECKPOINT_UNAVAILABLE" });
  assert.equal(existsSync(multipartDirectory), false);
  assert.deepEqual(snapshotFiles(lostMultipartOptions.recoveryDirectory), beforeMultipartReopen);
  assert.equal(existsSync(capture) ? readFileSync(capture, "utf8") : null, beforeLostReopen);
  authority = new EncryptedRemoteSession(firstOptions);
  const beforePlacementCheck = existsSync(capture) ? readFileSync(capture, "utf8") : null;
  // Choosing a host-storage ancestor as the worktree would expose checkpoints,
  // recovery slots and ciphertext staging to repository inventory/publication.
  assert.throws(() => new RepositorySession(directory, { encryptedRemote: authority }), { code: "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING" });
  assert.throws(() => new RepositorySession(firstOptions.recoveryDirectory, { encryptedRemote: authority }), { code: "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING" });
  assert.throws(() => new RepositorySession(join(firstOptions.stagingDirectory, "future-worktree"), { encryptedRemote: authority }), { code: "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING" });
  assert.equal(existsSync(capture) ? readFileSync(capture, "utf8") : null, beforePlacementCheck);
  assert.equal(authority.state, "locked");
  assert.throws(() => new EncryptedRemoteSession({ ...firstOptions, accountSubject: "other-account", createCheckpoint: false }), { code: "GRAFT_SDK_E2EE_IDENTITY_CHANGED" });
  assert.equal(JSON.stringify(authority), "{}");
  await assert.rejects(authority.unlockRemembered(), { code: "GRAFT_SDK_E2EE_SECURE_STORAGE_UNAVAILABLE" });
  const invalid = Buffer.from([0xff]);
  await assert.rejects(authority.unlock(invalid), { code: "GRAFT_SDK_E2EE_INVALID_PASSWORD" });
  assert.deepEqual([...invalid], [0]);
  const wrong = Buffer.from("incorrect synthetic password");
  await assert.rejects(authority.unlock(wrong), { code: "GRAFT_SDK_E2EE_UNLOCK_FAILED" });
  assert.ok(wrong.every((value) => value === 0));
  assert.equal(authority.state, "locked");
  const cancelledUnlock = authority.unlock(Buffer.from("node synthetic sync password"));
  const cancellation = assert.rejects(cancelledUnlock, { name: "AbortError" });
  await authority.lock();
  await cancellation;
  assert.equal(authority.state, "locked");
  const password = Buffer.from("node synthetic sync password");
  const unlocking = authority.unlock(password);
  assert.ok(password.every((value) => value === 0));
  assert.equal(authority.state, "unlocking");
  await unlocking;
  assert.equal(authority.state, "unlocked");
  assert.throws(() => authority.refreshBearerToken("native.test", "other-account", "renewed-account-token"), { code: "GRAFT_SDK_E2EE_IDENTITY_CHANGED" });
  authority.refreshBearerToken("native.test", "test-account", "renewed-account-token");
  const sourceDirectory = privateDirectory("source");
  source = new RepositorySession(sourceDirectory, { encryptedRemote: authority });
  await source.open();
  await source.init();
  await source.addEncryptedRemote();
  const configuredRemotes = (await source.listRemotes()).remotes;
  await assert.rejects(source.addEncryptedRemote());
  assert.deepEqual((await source.listRemotes()).remotes, configuredRemotes);
  db = new DatabaseSync(join(sourceDirectory, "private-records.sqlite"));
  db.exec("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE records(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO records VALUES (1, 'node private WAL value');");
  const payload = "node private attachment marker\n".repeat(4096);
  writeFileSync(join(sourceDirectory, "private-note.txt"), payload);
  await source.addAll();
  const committed = await source.commit("node private commit message");
  const head = committed.commit.id;
  let currentSourceHead = head;
  await assert.rejects(source.push({ remote: "origin", branch: "main" }), { code: "GRAFT_SDK_REMOTE_PUBLICATION_UNCONFIRMED" });
  const rejectedHeads = JSON.parse(readFileSync(capture)).filter((request) => request.rejectedInitialHeadCreate === true && request.path.startsWith("/acme/integration/"));
  assert.equal(rejectedHeads.length, 3);
  assert.ok(rejectedHeads.every((request) => request.body === rejectedHeads[0].body));
  await source.close();
  await authority.disconnect();
  // Drop both native handles and reopen the durable checkpoint and ref slots.
  authority = new EncryptedRemoteSession({ ...firstOptions, createCheckpoint: false });
  await authority.unlock(Buffer.from("node synthetic sync password"));
  source = new RepositorySession(sourceDirectory, { encryptedRemote: authority });
  await source.open();
  const beforeResume = JSON.parse(readFileSync(capture)).length;
  const resumedPush = await source.push({ remote: "origin", branch: "main" });
  assert.equal(resumedPush.commits, 0);
  const recoveredRequests = JSON.parse(readFileSync(capture));
  const headRequests = recoveredRequests.filter((request) => request.method === "POST" && request.path.startsWith("/acme/integration/cas/e2ee/head/"));
  assert.equal(headRequests.length, 4);
  assert.ok(headRequests.every((request) => request.body === rejectedHeads[0].body));
  const isBranchPublication = (request) => request.method === "POST" && (
    request.path.startsWith("/acme/integration/cas/e2ee/refs/") ||
    request.path.startsWith("/acme/integration/e2ee/receive-bundle/e2ee/refs/")
  );
  assert.equal(recoveredRequests.filter(isBranchPublication).length, 1);
  assert.equal(recoveredRequests.slice(beforeResume).filter(isBranchPublication).length, 0);
  assert.ok(recoveredRequests.slice(beforeResume).every((request) => request.method !== "PUT"));
  await source.close();
  assert.equal(authority.state, "unlocked");
  await source.open();
  assert.equal((await source.listRemotes()).remotes[0].kind, "encrypted_http");
  await authority.lock();
  assert.equal(authority.state, "locked");
  const before = JSON.parse(readFileSync(capture)).length;
  await assert.rejects(source.push({ remote: "origin", branch: "main" }), { code: "GRAFT_SDK_E2EE_LOCKED" });
  await assert.rejects(source.fetch({ remote: "origin", branch: "main" }), { code: "GRAFT_SDK_E2EE_LOCKED" });
  assert.equal(JSON.parse(readFileSync(capture)).length, before);
  await authority.unlock(Buffer.from("node synthetic sync password"));
  peerAuthority = new EncryptedRemoteSession(options("peer"));
  const peerDirectory = join(directory, "peer");
  peer = new RepositorySession(peerDirectory, { encryptedRemote: peerAuthority });
  await peer.open();
  const beforeLockedClone = JSON.parse(readFileSync(capture)).length;
  await assert.rejects(peer.cloneEncryptedRemote({ branch: "main" }), { code: "GRAFT_SDK_E2EE_LOCKED" });
  assert.equal(JSON.parse(readFileSync(capture)).length, beforeLockedClone);
  await peerAuthority.unlock(Buffer.from("node synthetic sync password"));
  await peer.cloneEncryptedRemote({ branch: "main" });
  assert.equal((await peer.repositoryMetadata()).current_head, head);
  const recovered = new DatabaseSync(join(peerDirectory, "private-records.sqlite"));
  assert.equal(recovered.prepare("SELECT value FROM records WHERE id=1").get().value, "node private WAL value");
  recovered.close();
  assert.equal(readFileSync(join(peerDirectory, "private-note.txt"), "utf8"), payload);
  await authority.disconnect();
  assert.equal(authority.state, "disconnected");
  await assert.rejects(authority.unlock(Buffer.from("node synthetic sync password")), { code: "GRAFT_SDK_E2EE_DISCONNECTED" });
  authority.reconnect("account-token");
  assert.equal(authority.state, "locked");
  await authority.unlock(Buffer.from("node synthetic sync password"));
  await source.push({ remote: "origin", branch: "main" });
  db.exec("UPDATE records SET value='node updated WAL value' WHERE id=1");
  await source.addAll();
  const updated = await source.commit("node updated private commit message");
  currentSourceHead = updated.commit.id;
  assert.notEqual(currentSourceHead, head);
  await source.push({ remote: "origin", branch: "main" });
  authority.pause(true);
  const beforePaused = JSON.parse(readFileSync(capture)).length;
  await assert.rejects(source.fetch({ remote: "origin", branch: "main" }), { code: "GRAFT_SDK_E2EE_PAUSED" });
  assert.equal(JSON.parse(readFileSync(capture)).length, beforePaused);
  assert.equal(authority.state, "unlocked");
  authority.pause(false);
  authority.refreshBearerToken("native.test", "test-account", "incorrect-account-token");
  const beforeUnauthorized = JSON.parse(readFileSync(capture)).length;
  await assert.rejects(source.fetch({ remote: "origin", branch: "main" }), { code: "GRAFT_SDK_E2EE_AUTHENTICATION_FAILED" });
  const unauthorizedRequests = JSON.parse(readFileSync(capture)).slice(beforeUnauthorized);
  assert.equal(unauthorizedRequests.length, 1);
  assert.equal(unauthorizedRequests[0].path, "/acme/integration");
  assert.equal(unauthorizedRequests[0].authenticated, false);
  assert.equal((await source.repositoryMetadata()).current_head, currentSourceHead);
  authority.refreshBearerToken("native.test", "test-account", "account-token");
  await source.fetch({ remote: "origin", branch: "main" });
  // Reopen the same durable checkpoint in a genuinely new locked native handle.
  await authority.disconnect();
  const reopened = new EncryptedRemoteSession({ ...firstOptions, createCheckpoint: false });
  assert.equal(reopened.state, "locked");
  await reopened.unlock(Buffer.from("node synthetic sync password"));
  await reopened.disconnect();
  if (process.platform === "darwin") {
    const unentitled = new EncryptedRemoteSession(options("unentitled", {
      keychainApplication: "org.graft.synthetic-node",
      keychainAccessGroup: "GRAFTTEST0.org.graft.synthetic-node",
    }));
    await unentitled.unlock(Buffer.from("node synthetic sync password"));
    await assert.rejects(unentitled.remember(), { code: "GRAFT_SDK_E2EE_SECURE_STORAGE_UNAVAILABLE" });
    await unentitled.lock();
    await assert.rejects(unentitled.unlockRemembered(), { code: "GRAFT_SDK_E2EE_SECURE_STORAGE_UNAVAILABLE" });
    await assert.rejects(unentitled.forget(), { code: "GRAFT_SDK_E2EE_SECURE_STORAGE_UNAVAILABLE" });
    assert.equal(unentitled.state, "disconnected");
    // Forget drains every session sharing the exact credential binding, even
    // when the provider refuses deletion. Further sync needs explicit admission.
    assert.equal(peerAuthority.state, "disconnected");
    peerAuthority.reconnect("account-token");
    await peerAuthority.unlock(Buffer.from("node synthetic sync password"));
  }
  // A malicious service can store a structurally valid but unauthenticated ref.
  // Exercise that through the actual handler, then restore the exact ciphertext.
  const refMutation = JSON.parse(readFileSync(capture)).find(isBranchPublication);
  assert.ok(refMutation);
  const refMutationPath = refMutation.path.replace("/e2ee/receive-bundle/", "/cas/");
  const origin = new URL(ready.integrationUrl).origin;
  const refResponse = await fetch(origin + refMutationPath.replace("/cas/", "/raw/"), { headers: { authorization: "Bearer account-token", "Graft-Protocol": "1" } });
  assert.equal(refResponse.status, 200);
  const refBytes = Buffer.from(await refResponse.arrayBuffer());
  const tampered = Buffer.from(refBytes);
  tampered[tampered.length - 1] ^= 1;
  async function replaceRef(expected, replacement) {
    const response = await fetch(origin + refMutationPath, { method: "POST", headers: { authorization: "Bearer account-token", "Graft-Protocol": "1", "Content-Type": "application/octet-stream", "x-graft-expected-present": "true", "x-graft-expected-hex": expected.toString("hex") }, body: replacement });
    assert.ok(response.ok, `ref CAS returned ${response.status}`);
    await response.arrayBuffer();
  }
  await replaceRef(refBytes, tampered);
  try {
    await assert.rejects(peer.fetch({ remote: "origin", branch: "main" }), { code: "GRAFT_SDK_E2EE_INTEGRITY_FAILED" });
    assert.equal((await peer.repositoryMetadata()).current_head, head);
  } finally { await replaceRef(tampered, refBytes); }
  await peer.fetch({ remote: "origin", branch: "main" });
  authority.reconnect("account-token");
  await authority.unlock(Buffer.from("node synthetic sync password"));
  const invalidCurrent = Buffer.from("node synthetic sync password");
  const invalidNext = Buffer.from([0xff]);
  await assert.rejects(authority.changePassword(invalidCurrent, invalidNext), { code: "GRAFT_SDK_E2EE_INVALID_PASSWORD" });
  assert.ok(invalidCurrent.every((value) => value === 0) && invalidNext.every((value) => value === 0));
  const wrongCurrent = Buffer.from("wrong node current password");
  const rejectedNext = Buffer.from("node changed sync password");
  await assert.rejects(authority.changePassword(wrongCurrent, rejectedNext), { code: "GRAFT_SDK_E2EE_UNLOCK_FAILED" });
  assert.ok(wrongCurrent.every((value) => value === 0) && rejectedNext.every((value) => value === 0));
  const currentPassword = Buffer.from("node synthetic sync password");
  const newPassword = Buffer.from("node changed sync password");
  const changing = authority.changePassword(currentPassword, newPassword);
  assert.ok(currentPassword.every((value) => value === 0) && newPassword.every((value) => value === 0));
  assert.equal(await changing, "unknown");
  const pendingCurrent = Buffer.from("node changed sync password");
  const pendingNext = Buffer.from("node later sync password");
  await assert.rejects(authority.changePassword(pendingCurrent, pendingNext), { code: "GRAFT_SDK_E2EE_STALE_OBSERVATION" });
  assert.ok(pendingCurrent.every((value) => value === 0) && pendingNext.every((value) => value === 0));
  await authority.disconnect();
  authority = new EncryptedRemoteSession({ ...firstOptions, createCheckpoint: false });
  await assert.rejects(authority.resumePasswordChange(), { code: "GRAFT_SDK_E2EE_LOCKED" });
  await assert.rejects(authority.unlock(Buffer.from("node synthetic sync password")), { code: "GRAFT_SDK_E2EE_UNLOCK_FAILED" });
  await authority.unlock(Buffer.from("node changed sync password"));
  const beforePasswordResume = JSON.parse(readFileSync(capture)).length;
  assert.equal(await authority.resumePasswordChange(), "confirmed");
  assert.ok(JSON.parse(readFileSync(capture)).slice(beforePasswordResume).every((request) => request.method === "GET"));
  await peer.fetch({ remote: "origin", branch: "main" });
  assert.equal(readFileSync(join(peerDirectory, "private-note.txt"), "utf8"), payload);
  const changedCurrent = Buffer.from("node changed sync password");
  const changedNext = Buffer.from("node later sync password");
  assert.equal(await authority.changePassword(changedCurrent, changedNext), "confirmed");
  assert.ok(changedCurrent.every((value) => value === 0) && changedNext.every((value) => value === 0));
  // A host can lose its final callback after native retirement. Reopening and
  // resuming the retained receipt must confirm by reading, without another CAS.
  await authority.disconnect();
  authority = new EncryptedRemoteSession({ ...firstOptions, createCheckpoint: false });
  await authority.unlock(Buffer.from("node later sync password"));
  const beforeReceiptResume = JSON.parse(readFileSync(capture)).length;
  assert.equal(await authority.resumePasswordChange(), "confirmed");
  const receiptRequests = JSON.parse(readFileSync(capture)).slice(beforeReceiptResume);
  assert.ok(receiptRequests.length > 0 && receiptRequests.every((request) => request.method === "GET"));
  const requests = JSON.parse(readFileSync(capture));
  assert.equal(requests.filter((request) => !request.authenticated).length, 2);
  assertLegacyRequestPrivacy(requests, ["node synthetic sync password", "node private WAL value", "node updated WAL value", "node private attachment marker", "node private commit message", "node updated private commit message", "private-records.sqlite", "private-note.txt", head, currentSourceHead,
    "node changed sync password", "node later sync password", "wrong node current password", "node random enrollment password", "wrong node enrollment password", "node replacement enrollment password", "node competing enrollment password", "node initial private attachment", "node initial private commit", "initial-node-private.txt", initialHead,
    Buffer.alloc(32, 42), ...[head, currentSourceHead, initialHead].map((id) => Buffer.from(id, "hex"))]);
  console.log("Native Node E2EE SQLite/WAL, password change/recovery, lifecycle and leakage checks passed");
} finally {
  clearTimeout(timeout);
  db?.close();
  await source?.close();
  await peer?.close();
  await authority?.disconnect();
  await peerAuthority?.disconnect();
  await enrollmentAuthority?.disconnect();
  await losingEnrollment?.disconnect();
  await initialSource?.close();
  await wrongInitialSource?.close();
  await namedPeer?.close();
  lines.close();
  if (service.exitCode === null && service.signalCode === null) {
    service.kill();
    await once(service, "exit");
  }
}
