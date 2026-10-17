// Small actual-native acceptance case; all values and credentials are synthetic.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { once } from "node:events";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { PrivacyEvidence, assertPrivacyEvidence, readPrivacyEvidence } from "./privacy-evidence.mjs";

const require = createRequire(import.meta.url);
const { EncryptedRemoteSession, RepositorySession } = require("../../packages/graft-sdk");
const [fixturePath, directory] = process.argv.slice(2);
const fixture = JSON.parse(readFileSync(fixturePath, "utf8"));
assert.equal(fixture.privacyEvidence, true);
const { nonce, password } = fixture.privacyCase;
const capture = join(directory, "privacy-requests.json");
const diagnostic = new PrivacyEvidence(join(directory, "service-diagnostics.jsonl"), { maxBytes: 1024 * 1024 });
const service = spawn(process.execPath, ["tests/e2ee/remote-service.mjs", fixturePath, capture], { stdio: ["ignore", "pipe", "pipe"] });
const serviceClosed = once(service, "close");
for (const [name, stream] of [["stdout", service.stdout], ["stderr", service.stderr]])
  stream.on("data", (chunk) => diagnostic.record("diagnostic", "service", name, {}, chunk));
const lines = createInterface({ input: service.stdout });
const timeout = setTimeout(() => { service.kill("SIGKILL"); throw new Error("Privacy acceptance timed out"); }, 120000);
const authorities = [];
const repositories = [];
const markers = [password, `file-${nonce}.txt`, `content-${nonce}`, `commit-${nonce}`, Buffer.alloc(32, 42)];
let completed = false;
try {
  const [line] = await Promise.race([once(lines, "line"), serviceClosed.then(() => {
    throw new Error("Synthetic privacy service exited before readiness");
  })]);
  const ready = JSON.parse(line);
  function privateDirectory(name) { const path = join(directory, name); mkdirSync(path, { mode: 0o700 }); return path; }
  function authority(name) {
    const value = new EncryptedRemoteSession({ remoteName: "origin", url: ready.integrationUrl,
      uuid: fixture.encryption.uuid, authenticationDomain: "native.test", accountSubject: "test-account", bearerToken: "account-token",
      checkpointDirectory: privateDirectory(`${name}-checkpoint`), recoveryDirectory: privateDirectory(`${name}-recovery`),
      stagingDirectory: privateDirectory(`${name}-staging`), createCheckpoint: true });
    authorities.push(value);
    return value;
  }
  const sender = authority("sender");
  await sender.unlock(Buffer.from(password));
  const sourcePath = privateDirectory("source");
  const source = new RepositorySession(sourcePath, { encryptedRemote: sender });
  repositories.push(source);
  await source.open();
  await source.init();
  await source.addEncryptedRemote();
  // Incompressible but bounded payload forces encrypted multipart at 32 KiB.
  const payload = Buffer.concat([Buffer.from(`content-${nonce}\n`), randomBytes(128 * 1024)]);
  writeFileSync(join(sourcePath, `file-${nonce}.txt`), payload);
  await source.addAll();
  const head = (await source.commit(`commit-${nonce}`)).commit.id;
  assert.match(head, /^[0-9a-f]{64}$/);
  markers.push(head, Buffer.from(head, "hex"));
  await source.push({ remote: "origin", branch: "main" });
  const receiver = authority("receiver");
  await receiver.unlock(Buffer.from(password));
  const target = new RepositorySession(join(directory, "restored"), { encryptedRemote: receiver });
  repositories.push(target);
  await target.open();
  await target.cloneEncryptedRemote({ branch: "main" });
  assert.equal((await target.repositoryMetadata()).current_head, head);
  const restored = readFileSync(join(directory, "restored", `file-${nonce}.txt`));
  assert.deepEqual(restored, payload);
  const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
  assert.equal(sha256(restored), sha256(payload));

  const headers = { authorization: "Bearer account-token", "Graft-Protocol": "1" };
  async function request(path, options = {}) {
    const response = await fetch(ready.integrationUrl + path, { ...options, headers: { ...headers, ...options.headers } });
    assert.ok(response.ok, `Synthetic privacy operation returned ${response.status}`);
    return response;
  }
  // Exercise deletion/abort instrumentation using already-encrypted bytes only.
  // These synthetic orphan operations do not claim another native upload path.
  const writes = JSON.parse(readFileSync(capture));
  const ref = writes.find((entry) => entry.method === "POST" && /\/(?:cas|e2ee\/receive-bundle)\/e2ee\/refs\//.test(entry.path));
  assert.ok(ref, "Native publication must create an opaque reference");
  const refKey = ref.path.slice(ref.path.indexOf("e2ee/refs/"));
  const encryptedRef = Buffer.from(await (await request(`/raw/${refKey}`)).arrayBuffer());
  await (await request(`/cad/${refKey}`, { method: "POST", headers: {
    "x-graft-expected-present": "true", "x-graft-expected-hex": encryptedRef.toString("hex") } })).arrayBuffer();
  const opaqueKey = `e2ee/objects/${randomBytes(32).toString("hex")}`;
  const uploadIdentity = { "x-graft-e2ee-uuid": fixture.encryption.uuid,
    "x-graft-e2ee-upload-id": randomBytes(32).toString("hex"),
    "x-graft-e2ee-ciphertext-sha256": sha256(encryptedRef) };
  const upload = await (await request(`/multipart-start/${opaqueKey}`, { method: "POST", headers: {
    ...uploadIdentity, "x-graft-object-bytes": String(encryptedRef.length) } })).json();
  await (await request(`/multipart-part/${opaqueKey}`, { method: "PUT", headers: {
    ...uploadIdentity, "x-graft-upload-id": upload.upload_id, "x-graft-part-number": "1",
    "content-type": "application/octet-stream" }, body: encryptedRef })).arrayBuffer();
  await (await request(`/multipart-abort/${opaqueKey}`, { method: "DELETE", headers: {
    ...uploadIdentity, "x-graft-upload-id": upload.upload_id } })).arrayBuffer();

  // The native loop is quiescent before the service finalizes its evidence.
  for (const repository of repositories.splice(0)) await repository.close();
  for (const value of authorities.splice(0)) await value.disconnect();
  service.kill();
  // "close" also drains both diagnostic pipes; "exit" alone can precede them.
  assert.deepEqual(await serviceClosed, [0, null]);
  diagnostic.finish();
  const records = readPrivacyEvidence(`${capture}.privacy.jsonl`);
  const logs = readPrivacyEvidence(diagnostic.path);
  assertPrivacyEvidence(records, markers);
  assertPrivacyEvidence(logs, markers, ["account-token", "renewed-account-token"]);
  const counts = {};
  for (const record of records) counts[`${record.surface}/${record.operation}`] = (counts[`${record.surface}/${record.operation}`] ?? 0) + 1;
  for (const category of ["request/request", "response/response", "storage/get", "storage/putIfAbsent", "storage/compareAndSwap-candidate", "storage/compareAndDelete", "multipart/uploadPart", "multipart/complete", "multipart/abort"])
    assert.ok(counts[category] > 0, `Privacy evidence missing ${category}`);
  assert.ok(records.some((record) => record.operation === "compareAndDelete" && record.byteLength > 0));
  assert.ok(records.some((record) => record.operation === "abort" && record.byteLength > 0));
  assert.ok(records.some((record) => record.surface === "storage" && record.metadata.key?.startsWith("e2ee/hints/")), "Expected public opaque routing hints must be present");
  assert.ok(records.some((record) => record.surface === "response" && record.byteLength > 0), "Actual response body required");
  const requests = records.filter((record) => record.surface === "request");
  const responses = records.filter((record) => record.surface === "response");
  assert.equal(requests.length, responses.length, "Every completed request must have a captured response");
  for (const request of requests) assert.ok(responses.some((response) => response.metadata.requestId === request.metadata.requestId));
  assert.ok(logs.length > 0, "Actual service process output required");
  // Local marker manifest is verifier input, not a remote/log surface.
  writeFileSync(join(directory, "privacy-markers.json"), JSON.stringify(markers.map((value) => Buffer.from(value).toString("hex"))), { mode: 0o600 });
  writeFileSync(join(directory, "privacy-summary.json"), JSON.stringify({ status: "passed", counts,
    restoredBytes: restored.length, restoredSha256: sha256(restored),
    remoteEvidenceSha256: sha256(readFileSync(`${capture}.privacy.jsonl`)),
    diagnosticsSha256: sha256(readFileSync(diagnostic.path)),
    allowedTopology: ["encrypted object classes and sizes", "opaque routing IDs", "authenticated public ancestry/pack hints", "synthetic HTTP authentication header"],
    limits: "One bounded native synthetic fixture; no claim of zero topology leakage or all-client safety" }, null, 2));
  completed = true;
  console.log("Native privacy acceptance: exact restored bytes; requests, responses, transient backend/multipart values and service diagnostics passed");
} finally {
  clearTimeout(timeout);
  for (const repository of repositories) await repository.close();
  for (const value of authorities) await value.disconnect();
  lines.close();
  if (service.exitCode === null && service.signalCode === null) {
    service.kill(completed ? "SIGTERM" : "SIGKILL");
  }
  await serviceClosed;
}
