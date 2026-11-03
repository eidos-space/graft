import assert from "node:assert/strict";
import { randomBytes, randomUUID, createHash } from "node:crypto";

function prefix(uuid, key, length, challenge) {
  const keyBytes = Buffer.from(key),
    keyLength = Buffer.alloc(4),
    size = Buffer.alloc(8);
  keyLength.writeUInt32BE(keyBytes.length);
  size.writeBigUInt64BE(BigInt(length));
  return Buffer.concat([
    Buffer.from("GRAFT-E2EE-OBJECT-PROOF-V1\0"),
    Buffer.from(uuid.replaceAll("-", ""), "hex"),
    keyLength,
    keyBytes,
    size,
    challenge,
  ]);
}

export async function proofHttp({ request, compose }) {
  const name = `proof-http-${randomUUID().slice(0, 8)}`;
  const remote = await (await request(`/api/graft/repositories/${name}`, { method: "PUT" })).json();
  const descriptor = await (await request(`/personal/${name}`)).json();
  if (!descriptor.capabilities.includes("e2ee-object-proof-v1")) {
    assert.ok(
      !process.env.GRAFT_REMOTE_SOURCE,
      "Candidate Remote did not advertise the implemented proof backend",
    );
    console.log("Object proof HTTP: not advertised by baseline Remote (not proof evidence)");
    return;
  }
  const key = `e2ee/files/${"a".repeat(64)}`;
  const bytes = Buffer.alloc(104);
  bytes.write("GRAFTEO1");
  bytes.writeUInt16BE(1, 8);
  bytes.writeUInt16BE(1, 10);
  bytes.writeUInt32BE(16, 68);
  assert.equal(
    (await request(`/personal/${name}/raw-if-not-exists/${key}`, { method: "PUT", body: bytes }))
      .status,
    204,
  );
  const path = `/personal/${name}/e2ee/object-proof/${key}`;
  const challenge = randomBytes(32);
  const ask = (body = challenge, extra = {}, target = path) =>
    request(target, {
      method: "POST",
      headers: {
        "Content-Type": "application/octet-stream",
        "X-Graft-Object-Bytes": String(bytes.length),
        ...extra,
      },
      body,
    });
  const first = await ask();
  assert.equal(first.status, 200, await first.clone().text());
  const value = await first.json();
  assert.equal(
    value.sha256,
    createHash("sha256")
      .update(prefix(remote.encryption.uuid, key, bytes.length, challenge))
      .update(bytes)
      .digest("hex"),
  );
  assert.equal(value.challenge, challenge.toString("hex"));
  assert.equal(value.key, key);
  assert.equal(value.uuid, remote.encryption.uuid);
  assert.equal(first.headers.get("cache-control"), "no-store");
  const second = await (await ask(randomBytes(32))).json();
  assert.notEqual(second.sha256, value.sha256);
  assert.equal((await ask(Buffer.alloc(31))).status, 400);
  assert.equal((await ask(challenge, { "X-Graft-Object-Bytes": "0104" })).status, 400);
  assert.equal((await ask(challenge, { "X-Graft-Object-Bytes": "105" })).status, 409);
  assert.equal((await ask(challenge, {}, path.replace("/files/", "/%66iles/"))).status, 400);
  assert.equal((await ask(challenge, {}, path.replace(/a{64}$/, "b".repeat(64)))).status, 404);
  assert.equal((await ask(challenge, { Authorization: "Bearer wrong" })).status, 401);
  // Deliberately damage only this isolated fixture's persisted BLOB inside Docker.
  const mutate = `import {DatabaseSync} from 'node:sqlite'; const db=new DatabaseSync('/data/graft.sqlite'); const row=db.prepare('SELECT value FROM objects WHERE repository=? AND path=?').get(process.argv[1],process.argv[2]); const bytes=Buffer.from(row.value); if(process.argv[3]==='flip')bytes[bytes.length-1]^=1; db.prepare('UPDATE objects SET value=? WHERE repository=? AND path=?').run(process.argv[3]==='truncate'?bytes.subarray(0,-1):bytes,process.argv[1],process.argv[2]); db.close();`;
  compose("exec", "-T", "graft", "node", "--input-type=module", "-e", mutate, name, key, "flip");
  assert.notEqual(
    (await (await ask()).json()).sha256,
    value.sha256,
    "proof must read actual damaged storage",
  );
  compose(
    "exec",
    "-T",
    "graft",
    "node",
    "--input-type=module",
    "-e",
    mutate,
    name,
    key,
    "truncate",
  );
  assert.equal((await ask()).status, 409);
  await request(`/api/graft/repositories/${name}`, { method: "DELETE" });
  assert.equal((await ask()).status, 404);
  console.log(
    "PASS: real Docker proof HTTP, challenge binding, encoded-path rejection, auth, persisted damage/truncation and deletion",
  );
}

// Frozen revision-1 vector, independently checks this test's prefix encoder.
assert.equal(
  createHash("sha256")
    .update(
      prefix(
        "00010203-0405-4607-8809-0a0b0c0d0e0f",
        `e2ee/files/${"a".repeat(64)}`,
        5,
        Buffer.from(Array.from({ length: 32 }, (_, i) => i)),
      ),
    )
    .update(Buffer.from("000102feff", "hex"))
    .digest("hex"),
  "4f904ed57dc3ecc8393ec4494eaa74bc6682a63a067043e3a689490d45e0532a",
);
