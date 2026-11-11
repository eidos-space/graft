import assert from "node:assert/strict";
import { randomUUID, randomBytes, createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";

export async function sdkRoundTrip({ origin, token, request, restart }) {
  const { RepositorySession, EncryptedRemoteSession } = createRequire(import.meta.url)(
    resolve(process.env.GRAFT_SDK_PATH ?? "packages/graft-sdk"),
  );
  const name = `sdk-smoke-${randomUUID().slice(0, 8)}`;
  const response = await request(`/api/graft/repositories/${name}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ encryption: "encrypted-remote-v1" }),
  });
  assert.equal(response.status, 201);
  const remote = await response.json();
  assert.equal(remote.remote_url, `${origin}/personal/${name}`);
  const root = await mkdtemp(join(tmpdir(), "graft-docker-sdk-"));
  const sessions = [];
  const authorities = [];
  const password = randomUUID();
  const identity = { name: "Graft Docker Smoke", email: "smoke@example.invalid" };

  async function authority(device) {
    const directories = {};
    for (const kind of ["checkpoint", "staging", "recovery"]) {
      const directory = join(root, `${device}-${kind}`);
      EncryptedRemoteSession.createPrivateDirectory(directory);
      directories[`${kind}Directory`] = directory;
    }
    const handle = new EncryptedRemoteSession({
      remoteName: "origin",
      url: remote.remote_url,
      uuid: remote.encryption.uuid,
      authenticationDomain: new URL(origin).hostname,
      accountSubject: "personal",
      bearerToken: token,
      createCheckpoint: true,
      ...directories,
    });
    authorities.push(handle);
    return handle;
  }

  try {
    const sourcePath = join(root, "source");
    await mkdir(sourcePath);
    const first = await authority("first");
    const source = await RepositorySession.open(sourcePath, { encryptedRemote: first, identity });
    sessions.push(source);
    await source.init();
    await source.addEncryptedRemote();
    const database = new DatabaseSync(join(sourcePath, "notes.sqlite"));
    database.exec(
      "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes VALUES (1, 'first device');",
    );
    const largeDatabaseMiB = Number(process.env.GRAFT_LARGE_DATABASE_MIB ?? 0);
    const databaseHash = createHash("sha256");
    if (largeDatabaseMiB) {
      database.exec("CREATE TABLE blocks(id INTEGER PRIMARY KEY, data BLOB)");
      const insert = database.prepare("INSERT INTO blocks VALUES(?,?)");
      for (let index = 0; index < largeDatabaseMiB; index++) {
        const bytes = randomBytes(1024 * 1024);
        databaseHash.update(bytes);
        insert.run(index, bytes);
      }
    }
    const databaseDigest = databaseHash.digest("hex");
    database.close();
    await writeFile(join(sourcePath, "attachment.txt"), "encrypted attachment");
    let largeDigest;
    const largeBytes = Number(process.env.GRAFT_LARGE_PAYLOAD_MIB ?? 0) * 1024 * 1024;
    if (largeBytes) {
      const payload = randomBytes(largeBytes);
      largeDigest = createHash("sha256").update(payload).digest("hex");
      await writeFile(join(sourcePath, "large.bin"), payload);
    }
    await source.addAll();
    await source.commit("Initial encrypted smoke data");
    const commit = (await source.repositoryMetadata()).current_head;
    assert.equal(
      await first.initializeConfig(Buffer.from(password), { branch: "main", commit }),
      "config_published",
    );
    const initialized = await source.pushInitialBranch({
      verificationDirectory: join(root, "verified"),
    });
    assert.equal(initialized.state, "connected");

    // Kill the process after acknowledged publication; clone must use durable state.
    await restart();
    const afterRestart = await (await request(`/personal/${name}`)).json();
    assert.equal(afterRestart.encryption.uuid, remote.encryption.uuid);
    const second = await authority("second");
    await assert.rejects(second.unlock(Buffer.from("incorrect smoke password")));
    await second.unlock(Buffer.from(password));
    const peerPath = join(root, "peer");
    const peer = await RepositorySession.open(peerPath, { encryptedRemote: second, identity });
    sessions.push(peer);
    await peer.cloneEncryptedRemote({ branch: "main" });
    assert.equal(await readFile(join(peerPath, "attachment.txt"), "utf8"), "encrypted attachment");
    if (largeDigest)
      assert.equal(
        createHash("sha256")
          .update(await readFile(join(peerPath, "large.bin")))
          .digest("hex"),
        largeDigest,
      );
    const peerDb = new DatabaseSync(join(peerPath, "notes.sqlite"));
    assert.equal(peerDb.prepare("SELECT body FROM notes WHERE id = 1").get().body, "first device");
    if (largeDatabaseMiB) {
      const hash = createHash("sha256");
      let count = 0;
      for (const row of peerDb.prepare("SELECT data FROM blocks ORDER BY id").iterate()) {
        hash.update(row.data);
        count++;
      }
      assert.equal(count, largeDatabaseMiB);
      assert.equal(hash.digest("hex"), databaseDigest);
    }
    peerDb.exec("UPDATE notes SET body = 'second device' WHERE id = 1");
    peerDb.close();
    await writeFile(join(peerPath, "attachment.txt"), "updated on second device");
    await peer.addAll();
    await peer.commit("Second encrypted device update");
    await peer.push({ remote: "origin", branch: "main" });
    await source.pull({ remote: "origin", branch: "main" });
    assert.equal(
      await readFile(join(sourcePath, "attachment.txt"), "utf8"),
      "updated on second device",
    );
    const restoredDb = new DatabaseSync(join(sourcePath, "notes.sqlite"));
    assert.equal(
      restoredDb.prepare("SELECT body FROM notes WHERE id = 1").get().body,
      "second device",
    );
    restoredDb.close();
    console.log(
      JSON.stringify({
        status: "passed",
        sdkVersion: createRequire(import.meta.url)(
          join(resolve(process.env.GRAFT_SDK_PATH ?? "packages/graft-sdk"), "package.json"),
        ).version,
        largePayloadBytes: largeBytes,
        largeDatabaseMiB,
        repository: name,
        checks: [
          "native E2EE initialization",
          "verified first push",
          "wrong password rejected",
          "independent device clone",
          "SQLite and attachment round trip",
        ],
      }),
    );
  } finally {
    const closed = await Promise.allSettled(sessions.reverse().map((session) => session.close()));
    const disconnected = await Promise.allSettled(
      authorities.reverse().map((handle) => handle.disconnect()),
    );
    await rm(root, { recursive: true, force: true });
    if ([...closed, ...disconnected].some((result) => result.status === "rejected")) {
      console.error("One or more smoke sessions failed to close cleanly");
      process.exitCode = 1;
    }
  }
}
