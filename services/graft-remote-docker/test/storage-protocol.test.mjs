import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { Storage } from "../storage.mjs";
import { createApp } from "../app.mjs";

// Public protocol framing only. Native cryptographic validation is covered by
// candidate-smoke and sdk-smoke, not these synthetic frames.
function integer(value, width) {
  const bytes = Buffer.alloc(width);
  if (width === 8) bytes.writeBigUInt64BE(BigInt(value));
  else bytes.writeUIntBE(value, 0, width);
  return bytes;
}
function fields(...values) {
  return Buffer.concat([
    integer(values.length, 2),
    ...values.flatMap((value) => [integer(value.length, 4), value]),
  ]);
}
function fixture() {
  const refKey = `e2ee/refs/${"a".repeat(64)}`;
  const replacement = Buffer.concat([
    Buffer.from("GRAFTER1"),
    integer(1, 2),
    integer(1, 2),
    Buffer.from([1]),
    Buffer.alloc(32, 1),
    Buffer.alloc(40),
  ]);
  const object = Buffer.alloc(104);
  object.write("GRAFTEO1");
  object[9] = object[11] = 1;
  object.writeUInt32BE(16, 68);
  const entries = ["1", "2"].map((digit) => ({
    key: `e2ee/objects/${digit.repeat(64)}`,
    value: object,
  }));
  const manifest = fields(
    integer(1, 2),
    Buffer.from(refKey),
    replacement,
    fields(...entries.map(({ key, value }) => fields(Buffer.from(key), integer(value.length, 8)))),
  );
  const body = Buffer.concat([
    Buffer.from("GRAFTEB1"),
    integer(manifest.length, 4),
    manifest,
    ...entries.flatMap(({ key, value }) => [
      integer(Buffer.byteLength(key), 4),
      integer(value.length, 8),
      Buffer.from(key),
      value,
    ]),
  ]);
  return { body, refKey, entries };
}

test("SQLite aggregate publication commits complete dependencies before ref; interruption leaves no ref", async (t) => {
  const root = mkdtempSync(join(tmpdir(), "graft-docker-bundle-"));
  const storage = new Storage(join(root, "graft.sqlite"));
  t.after(() => {
    storage.close();
    rmSync(root, { recursive: true });
  });
  assert.equal(storage.db.prepare("PRAGMA journal_mode").get().journal_mode, "wal");
  assert.equal(storage.db.prepare("PRAGMA synchronous").get().synchronous, 2);
  const row = storage.provision("bundle", "Bundle", true).row;
  const backend = storage.backend(row);
  const token = randomBytes(32).toString("hex");
  const app = createApp({ storage, origin: "http://127.0.0.1:8787", token });
  const { body, refKey, entries } = fixture();
  const send = (value) =>
    app(
      new Request(`http://127.0.0.1:8787/personal/bundle/e2ee/receive-bundle/${refKey}`, {
        method: "POST",
        headers: {
          Authorization: `Bearer ${token}`,
          "Graft-Protocol": "1",
          "Content-Type": "application/vnd.graft.e2ee-bundle",
          "Content-Length": String(body.length),
          "x-graft-expected-present": "false",
          "x-graft-expected-hex": "",
        },
        body: value,
      }),
    );
  const interrupted = await send(body.subarray(0, -20));
  assert.equal(interrupted.status, 400);
  assert.equal(backend.head(refKey), null);
  assert.ok(backend.head(entries[0].key), "complete prefix object can remain safely orphaned");
  assert.equal(backend.head(entries[1].key), null, "partial frame must not publish");
  // A trigger observes actual SQLite state at ref publication, not in-memory mocks.
  storage.db.exec(`CREATE TRIGGER require_dependencies BEFORE INSERT ON objects
    WHEN NEW.path = '${refKey}' AND
      (SELECT COUNT(*) FROM objects WHERE repository=NEW.repository AND path IN ('${entries[0].key}', '${entries[1].key}')) != 2
    BEGIN SELECT RAISE(ABORT, 'dependencies missing'); END;`);
  const retry = await send(body);
  assert.equal(retry.status, 204, await retry.text());
  assert.ok(backend.head(refKey));
  for (const entry of entries)
    assert.deepEqual(Buffer.from(backend.get(entry.key).body), entry.value);
  assert.equal((await send(body)).status, 409, "create-only ref retry must not overwrite");
});
