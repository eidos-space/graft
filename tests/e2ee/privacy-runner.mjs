// Capture actual native-client stdout/stderr separately from legitimate API data.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { PrivacyEvidence, assertPrivacyEvidence, readPrivacyEvidence } from "./privacy-evidence.mjs";

const [fixture, directory] = process.argv.slice(2);
const artifactDirectory = join(process.env.GRAFT_E2EE_PRIVACY_EVIDENCE_DIR ?? "target/e2ee-privacy-evidence",
  `${process.platform}-${process.arch}-node${process.versions.node}-${process.pid}`);
let passed = false;
const saveProcessResult = (name, result) => {
  writeFileSync(join(directory, `${name}-stdout.bin`), result.stdout ?? Buffer.alloc(0), { mode: 0o600 });
  writeFileSync(join(directory, `${name}-stderr.bin`), result.stderr ?? Buffer.alloc(0), { mode: 0o600 });
  writeFileSync(join(directory, `${name}-execution.json`), JSON.stringify({ status: result.status, signal: result.signal,
    captureComplete: result.error === undefined, errorCode: result.error?.code ?? null,
    cleanupComplete: result.cleanupError === undefined, cleanupErrorCode: result.cleanupError?.code ?? null }), { mode: 0o600 });
};
const runCaptured = (arguments_, timeout) => {
  const detached = process.platform !== "win32";
  const result = spawnSync(process.execPath, arguments_, {
    maxBuffer: 1024 * 1024, timeout, encoding: "buffer", detached,
  });
  // Timeout/buffer overflow can stop the direct child before its cleanup runs.
  // Its dedicated Unix process group also owns the synthetic service children.
  if (detached && result.pid && (result.error || result.status !== 0)) {
    try { process.kill(-result.pid, "SIGKILL"); }
    catch (error) { if (error.code !== "ESRCH") result.cleanupError = error; }
  }
  return result;
};
try {
  const helpers = runCaptured(["--test", "tests/e2ee/privacy-evidence.test.mjs", "tests/e2ee/privacy-service.test.mjs"], 30000);
  saveProcessResult("observer", helpers);
  process.stdout.write(helpers.stdout ?? Buffer.alloc(0));
  process.stderr.write(helpers.stderr ?? Buffer.alloc(0));
  assert.equal(helpers.cleanupError, undefined, "Privacy observer process-group cleanup failed");
  assert.equal(helpers.error, undefined, "Privacy observer regression runner failed");
  assert.equal(helpers.status, 0, "Privacy observer regressions failed");
  const child = runCaptured(["tests/e2ee/node-privacy.mjs", fixture, directory], 150000);
  saveProcessResult("client", child);
  assert.equal(child.cleanupError, undefined, "Native privacy process-group cleanup failed");
  assert.equal(child.error, undefined, "Native privacy process timed out or exceeded diagnostic capture budget");
  // On failure retain the real diagnostics in CI; do not convert failures to pass.
  if (child.status !== 0) {
    process.stdout.write(child.stdout ?? Buffer.alloc(0));
    process.stderr.write(child.stderr ?? Buffer.alloc(0));
  }
  assert.equal(child.status, 0, "Native privacy client failed");
  const markers = JSON.parse(readFileSync(join(directory, "privacy-markers.json"))).map((hex) => Buffer.from(hex, "hex"));
  const evidence = new PrivacyEvidence(join(directory, "client-diagnostics.jsonl"), { maxBytes: 2 * 1024 * 1024 });
  evidence.record("diagnostic", "native-client", "stdout", {}, child.stdout);
  evidence.record("diagnostic", "native-client", "stderr", {}, child.stderr);
  evidence.finish();
  assertPrivacyEvidence(readPrivacyEvidence(evidence.path), markers, ["account-token", "renewed-account-token"]);
  process.stdout.write(child.stdout);
  console.log("Actual native client stdout/stderr privacy scan passed");
  passed = true;
} finally {
  // Evidence survives the Rust driver's temporary directory cleanup. Only
  // bounded synthetic remote/log captures are exported, never verifier inputs,
  // local worktrees, root/password manifests or private native journals.
  mkdirSync(artifactDirectory, { recursive: true, mode: 0o700 });
  let total = 0;
  const retained = [];
  let exportError;
  try {
    for (const name of ["privacy-requests.json", "privacy-requests.json.privacy.jsonl", "privacy-summary.json",
      "service-diagnostics.jsonl", "client-diagnostics.jsonl", "client-stdout.bin", "client-stderr.bin", "client-execution.json",
      "observer-stdout.bin", "observer-stderr.bin", "observer-execution.json"]) {
      const path = join(directory, name);
      if (!existsSync(path)) continue;
      const bytes = statSync(path).size;
      assert.ok(total + bytes <= 64 * 1024 * 1024, "Retained privacy evidence exceeds bounded export budget");
      copyFileSync(path, join(artifactDirectory, name));
      total += bytes;
      retained.push({ name, bytes, sha256: createHash("sha256").update(readFileSync(path)).digest("hex") });
    }
  } catch (error) {
    exportError = error;
  } finally {
    writeFileSync(join(artifactDirectory, "acceptance.json"), JSON.stringify({ status: passed && !exportError ? "passed" : "failed",
      exportComplete: exportError === undefined, bytes: total, retained, syntheticOnly: true, markerManifestExported: false }, null, 2));
    console.log(`Synthetic privacy evidence retained: ${artifactDirectory}`);
  }
  if (exportError) throw exportError;
}
