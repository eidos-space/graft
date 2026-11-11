import test from "node:test";
import assert from "node:assert/strict";
import { headerProbe, payloadRead } from "./fault-proxy.mjs";

test("only exact paired authenticated header ranges are excluded from payload reads", () => {
  const headers = new Map();
  const path = `/personal/test/raw/e2ee/files/${"a".repeat(64)}`;
  const first = Buffer.alloc(72);
  first.writeUInt32BE(184, 68);
  const row = { method: "GET", status: 206, path, range: "bytes=0-71" };
  assert.equal(headerProbe(row, first, "bytes 0-71/83906816", headers), true);
  const second = { ...row, range: "bytes=72-255" };
  assert.equal(headerProbe(second, Buffer.alloc(184), "bytes 72-255/83906816", headers), true);
  assert.equal(headerProbe(second, Buffer.alloc(184), "bytes 72-255/83906816", new Map()), false);
  assert.equal(headerProbe(second, Buffer.alloc(183), "bytes 72-255/83906816", headers), false);
  assert.equal(headerProbe(second, Buffer.alloc(184), "bytes 72-255/83906817", headers), false);
  const payload = { ...row, range: "bytes=256-271" };
  assert.equal(headerProbe(payload, Buffer.alloc(16), "bytes 256-271/83906816", headers), false);
  assert.equal(payloadRead(payload), true);
  assert.equal(payloadRead({ ...row, status: 200 }), true);
  assert.equal(payloadRead({ ...row, headerProbe: true }), false);
});
