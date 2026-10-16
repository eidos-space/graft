import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { PrivacyEvidence, assertPrivacyEvidence, assertLegacyRequestPrivacy, readPrivacyEvidence as readComplete } from "./privacy-evidence.mjs";

const readPrivacyEvidence = (path) => readComplete(path, { allowOpen: true });

const marker = "unique synthetic private marker 93c7";
const rootKey = Buffer.alloc(32, 42);
const withEvidence = async (work, options) => {
  const directory = mkdtempSync(join(tmpdir(), "graft-privacy-helper-"));
  const path = join(directory, "evidence.jsonl");
  try { await work(new PrivacyEvidence(path, options), path); }
  finally { rmSync(directory, { recursive: true, force: true }); }
};

test("records complete bytes, sequence, hash and transient deletion history", () => withEvidence((evidence, path) => {
  evidence.record("storage", "repo", "put", { key: "e2ee/objects/opaque" }, Buffer.from(marker));
  evidence.record("storage", "repo", "delete", { key: "e2ee/objects/opaque" }, Buffer.from(marker));
  const records = readPrivacyEvidence(path);
  assert.equal(records.length, 2);
  assert.throws(() => assertPrivacyEvidence(records, [marker]), /Private marker/);
}));

for (const [name, bytes] of [["UTF8", Buffer.from(marker)], ["hex", Buffer.from(Buffer.from(marker).toString("hex"))], ["base64", Buffer.from(Buffer.from(marker).toString("base64"))], ["raw root", rootKey]])
  test(`detects ${name} leaks in each remote surface`, async () => {
    for (const surface of ["request", "response", "storage", "multipart", "diagnostic"])
      await withEvidence((evidence, path) => {
        evidence.record(surface, "repo", "test", {}, bytes);
        assert.throws(() => assertPrivacyEvidence(readPrivacyEvidence(path), [marker, rootKey]), /Private marker/);
      });
  });

test("HTTP auth may appear on wire but never in diagnostics", () => withEvidence((evidence, path) => {
  evidence.record("request", "repo", "GET", { authorization: "Bearer account-token" });
  assertPrivacyEvidence(readPrivacyEvidence(path), [marker], ["account-token"]);
  evidence.record("diagnostic", "process", "stderr", {}, Buffer.from("Bearer account-token"));
  assert.throws(() => assertPrivacyEvidence(readPrivacyEvidence(path), [marker], ["account-token"]), /Private marker/);
}));

test("captures actual response headers/body without consuming the original", () => withEvidence(async (evidence, path) => {
  const response = new Response("opaque ciphertext", { headers: { "x-synthetic": marker } });
  await evidence.captureHttp("response", "repo", response, 1);
  assert.equal(await response.text(), "opaque ciphertext");
  assert.throws(() => assertPrivacyEvidence(readPrivacyEvidence(path), [marker]), /metadata/);
}));

test("budget overflow leaves explicit failed evidence, never a passing prefix", () => withEvidence(async (evidence, path) => {
  await assert.rejects(evidence.captureHttp("response", "repo", new Response("x".repeat(33)), 1), /budget/);
  assert.throws(() => readPrivacyEvidence(path), /capture failed/);
}, { maxBytes: 32 }));

test("record-count overflow fails closed", () => withEvidence((evidence, path) => {
  evidence.record("storage", "repo", "put");
  assert.throws(() => evidence.record("storage", "repo", "put"), /budget/);
  assert.throws(() => readPrivacyEvidence(path), /capture failed/);
}, { maxRecords: 1 }));

test("missing tail, sequence, digest and body truncation are rejected", async () => {
  for (const mutate of [
    (value) => value.slice(0, -1),
    (value) => value.replace('"sequence":1', '"sequence":2'),
    (value) => value.replace('"byteLength":4', '"byteLength":5'),
    (value) => value.replace('"bodyHex":"73616665"', '"bodyHex":"73616664"'),
  ]) await withEvidence((evidence, path) => {
    evidence.record("storage", "repo", "put", {}, Buffer.from("safe"));
    writeFileSync(path, mutate(readFileSync(path, "utf8")));
    assert.throws(() => readPrivacyEvidence(path));
  });
});

test("legacy request scanner catches raw object IDs inside hex bodies", () => {
  const id = Buffer.from("0193ab".repeat(10) + "ab12", "hex");
  assert.throws(() => assertLegacyRequestPrivacy([{ body: Buffer.concat([Buffer.from("prefix"), id]).toString("hex") }], [id]), /Private marker/);
});

test("complete evidence requires a final chain digest and rejects removed whole records", () => withEvidence((evidence, path) => {
  evidence.record("storage", "repo", "put", {}, Buffer.from("safe"));
  assert.throws(() => readComplete(path), /completion footer/);
  evidence.finish();
  assert.equal(readComplete(path).length, 1);
  const lines = readFileSync(path, "utf8").split("\n");
  writeFileSync(path, lines.slice(1).join("\n"));
  assert.throws(() => readComplete(path), /tail missing/);
}));

test("split markers cannot evade diagnostic or multipart-part scans", async () => {
  for (const surface of ["diagnostic", "multipart"])
    await withEvidence((evidence, path) => {
      for (const body of [marker.slice(0, 13), marker.slice(13)])
        evidence.record(surface, "repo", surface === "multipart" ? "uploadPart" : "stderr", { uploadId: "same-upload" }, Buffer.from(body));
      assert.throws(() => assertPrivacyEvidence(readPrivacyEvidence(path), [marker]), /reassembled/);
    });
});

test("metadata markers and dishonest final byte counts cannot pass", async () => {
  for (const surface of ["request", "response", "storage", "multipart", "diagnostic"])
    await withEvidence((evidence, path) => {
      evidence.record(surface, "repo", "test", { field: marker });
      evidence.finish();
      assert.throws(() => assertPrivacyEvidence(readComplete(path), [marker]), /metadata/);
      const lines = readFileSync(path, "utf8").trimEnd().split("\n");
      const footer = JSON.parse(lines.pop());
      lines.push(JSON.stringify({ ...footer, bytes: footer.bytes + 1 }));
      writeFileSync(path, lines.join("\n") + "\n");
      assert.throws(() => readComplete(path), /total byte count/);
    });
});
