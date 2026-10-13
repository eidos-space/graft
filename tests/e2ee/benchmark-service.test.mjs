import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";

test("benchmark counters count actual body bytes and reset between snapshots", async () => {
  const directory = mkdtempSync(join(tmpdir(), "graft-metrics-"));
  const fixture = join(directory, "fixture.json");
  writeFileSync(fixture, JSON.stringify({ config: [], encryption: { profile: "encrypted-remote-v1", version: 1, suite: 1, uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f" } }));
  const child = spawn("node", ["tests/e2ee/benchmark-service.mjs", fixture], { stdio: ["pipe", "pipe", "inherit"] });
  const lines = createInterface({ input: child.stdout });
  const read = async () => JSON.parse((await once(lines, "line"))[0]);
  const snapshot = async () => { const waiting = read(); child.stdin.write("snapshot\n"); return waiting; };
  try {
    const { origin } = await read();
    const headers = { authorization: "Bearer benchmark-token", "Graft-Protocol": "1" };
    const descriptor = await fetch(`${origin}/plain`, { headers });
    assert.equal(descriptor.status, 200);
    const bytes = await descriptor.arrayBuffer();
    assert.deepEqual(await snapshot(), { requests: 1, request_body_bytes: 0, response_body_bytes: bytes.byteLength, operation_counts: { descriptor: 1 } });
    const response = await fetch(`${origin}/plain/raw-if-not-exists/objects/${"a".repeat(64)}`, { method: "PUT", headers, body: "counter" });
    assert.equal(response.status, 204);
    await response.arrayBuffer();
    assert.deepEqual(await snapshot(), { requests: 1, request_body_bytes: 7, response_body_bytes: 0, operation_counts: { "raw-if-not-exists": 1 } });
    const range = await fetch(`${origin}/plain/raw/objects/${"a".repeat(64)}`, { headers: { ...headers, Range: "bytes=1-3" } });
    assert.equal(range.status, 206);
    assert.equal(range.headers.get("content-range"), "bytes 1-3/7");
    assert.equal(await range.text(), "oun");
    assert.deepEqual(await snapshot(), { requests: 1, request_body_bytes: 0, response_body_bytes: 3, operation_counts: { raw: 1 } });
    assert.deepEqual(await snapshot(), { requests: 0, request_body_bytes: 0, response_body_bytes: 0, operation_counts: {} });
  } finally { lines.close(); child.kill(); await once(child, "exit"); rmSync(directory, { recursive: true, force: true }); }
});
