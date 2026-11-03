import assert from "node:assert/strict";
import { createHash, randomBytes, randomUUID } from "node:crypto";
import { createRequire } from "node:module";
import { readFile, writeFile, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { DatabaseSync } from "node:sqlite";
import { faultProxy, payloadPath, payloadRead } from "./fault-proxy.mjs";

// Set native override before requiring the candidate; old SDK runs in another process.
assert.ok(process.env.GRAFT_CANDIDATE_SDK_PATH, "An explicit candidate module is required");
assert.ok(
  process.env.GRAFT_CANDIDATE_NATIVE_PATH,
  "An explicit candidate native binary is required",
);
process.env.GRAFT_SDK_NATIVE_PATH = resolve(process.env.GRAFT_CANDIDATE_NATIVE_PATH);
const modulePath = resolve(process.env.GRAFT_CANDIDATE_SDK_PATH);
const require = createRequire(import.meta.url);
const { RepositorySession, EncryptedRemoteSession } = require(modulePath);
const nativeHash = createHash("sha256")
  .update(await readFile(process.env.GRAFT_SDK_NATIVE_PATH))
  .digest("hex");
const version = require(join(modulePath, "package.json")).version;
const root = await mkdtemp(join(tmpdir(), "graft-docker-candidate-"));
const identity = { name: "Docker candidate test", email: "test@example.invalid" };
const summaries = [];

export async function candidateRoundTrips({ origin, token, request, restart }) {
  async function scenario(fault) {
    const name = `candidate-${randomUUID().slice(0, 8)}`;
    const created = await request(`/api/graft/repositories/${name}`, { method: "PUT" });
    assert.equal(created.status, 201);
    const remote = await created.json();
    const proxy = await faultProxy(origin, { fallback: fault !== "none", restart });
    const password = randomUUID();
    const privateDirectory = (label) => {
      const path = join(root, `${name}-${label}`);
      EncryptedRemoteSession.createPrivateDirectory(path);
      return path;
    };
    const options = {
      remoteName: "origin",
      url: `${proxy.origin}/personal/${name}`,
      uuid: remote.encryption.uuid,
      authenticationDomain: "127.0.0.1",
      accountSubject: "personal",
      bearerToken: token,
      checkpointDirectory: privateDirectory("checkpoint"),
      stagingDirectory: privateDirectory("staging"),
      recoveryDirectory: privateDirectory("recovery"),
      createCheckpoint: true,
    };
    const sourcePath = privateDirectory("source");
    let authority = new EncryptedRemoteSession(options);
    let source = await RepositorySession.open(sourcePath, { encryptedRemote: authority, identity });
    let peer, peerAuthority;
    try {
      await source.init();
      await source.addEncryptedRemote();
      const payload = randomBytes(
        (fault === "none" ? Number(process.env.GRAFT_LARGE_PAYLOAD_MIB ?? 2) : 2) * 1024 * 1024,
      );
      await writeFile(join(sourcePath, "payload.bin"), payload);
      const db = new DatabaseSync(join(sourcePath, "notes.sqlite"));
      db.exec(
        "CREATE TABLE notes(id INTEGER PRIMARY KEY, body TEXT); INSERT INTO notes VALUES(1, 'candidate data');",
      );
      const databaseMiB = fault === "none" ? Number(process.env.GRAFT_LARGE_DATABASE_MIB ?? 0) : 0;
      const databaseHash = createHash("sha256");
      if (databaseMiB) {
        db.exec("CREATE TABLE blocks(id INTEGER PRIMARY KEY, data BLOB)");
        const insert = db.prepare("INSERT INTO blocks VALUES(?,?)");
        for (let i = 0; i < databaseMiB; i++) {
          const bytes = randomBytes(1024 * 1024);
          databaseHash.update(bytes);
          insert.run(i, bytes);
        }
      }
      const databaseDigest = databaseHash.digest("hex");
      db.close();
      await source.addAll();
      await source.commit("Candidate initial data");
      const commit = (await source.repositoryMetadata()).current_head;
      await authority.initializeConfig(Buffer.from(password), { branch: "main", commit });
      const start = proxy.records.length;
      const phases = [];
      let confirmingAt;
      let initial;
      let failed = false;
      if (fault === "client-crash") {
        await source.close();
        await authority.disconnect();
        source = authority = undefined;
        const child = spawn(
          process.execPath,
          ["services/graft-remote-docker/test/candidate-client.mjs"],
          { stdio: ["pipe", "pipe", "inherit"] },
        );
        proxy.arm(fault, child);
        child.stdin.end(
          JSON.stringify({
            modulePath,
            options: { ...options, createCheckpoint: false },
            sourcePath,
            password,
          }),
        );
        const [code, signal] = await once(child, "exit");
        assert.equal(code, null);
        assert.equal(signal, "SIGKILL");
        failed = true;
      } else {
        proxy.arm(fault);
        try {
          initial = await source.pushInitialBranch({
            onSyncProgress: (event) => {
              phases.push(event.phase);
              if (event.phase === "confirming") confirmingAt ??= proxy.records.length;
            },
          });
        } catch {
          failed = true;
        }
      }
      await proxy.recovered;
      if (fault === "none") {
        assert.equal(failed, false);
        assert.equal(initial.state, "connected");
        assert.equal(initial.verification, "publication");
        assert.ok(phases.includes("confirming"));
        assert.ok(phases.includes("completed"));
        const confirmed = proxy.records
          .slice(confirmingAt)
          .filter((row) => row.method === "GET" && row.status === 200);
        for (const pattern of [
          /^\/personal\/[^/]+$/,
          /\/e2ee\/config$/,
          /\/e2ee\/refs\//,
          /\/e2ee\/head\//,
        ]) {
          assert.ok(
            confirmed.some((row) => pattern.test(row.path)),
            `fresh confirmation missing ${pattern}`,
          );
        }
      } else {
        assert.ok(proxy.injected > 0, `${fault}: injection did not execute`);
        if (fault === "interrupt-upload" || fault === "client-crash") assert.equal(failed, true);
        if (failed) assert.ok(!phases.includes("completed"));
      }
      const firstAttempt = proxy.records.slice(start);
      if (fault === "none")
        assert.equal(
          firstAttempt.filter(payloadRead).length,
          0,
          `${fault}: default initialization downloaded payloads: ${JSON.stringify(firstAttempt.filter(payloadRead))}`,
        );
      if (fault === "interrupt-upload") {
        assert.equal(
          firstAttempt.some(
            (row) => ["PUT", "POST"].includes(row.method) && /\/e2ee\/(refs|head)\//.test(row.path),
          ),
          false,
          "ref published before dependencies",
        );
        for (const row of firstAttempt.filter((row) => row.truncated)) {
          const result = await request(row.path.replace("/raw-if-not-exists/", "/raw/"));
          assert.equal(result.status, 404, "partial immutable object became visible");
          await result.body?.cancel();
        }
      }
      // Read back a lost-response object directly, proving it was really committed.
      for (const row of firstAttempt.filter((row) => row.responseLost && payloadPath(row.path))) {
        const response = await request(row.path.replace("/raw-if-not-exists/", "/raw/"));
        assert.equal(response.status, 200);
        assert.equal(
          createHash("sha256")
            .update(Buffer.from(await response.arrayBuffer()))
            .digest("hex"),
          row.uploadHash,
        );
      }
      await source?.close();
      await authority?.disconnect();
      source = authority = undefined;
      proxy.clear();
      await restart(); // Includes successful publication and failure recovery after server SIGKILL.
      authority = new EncryptedRemoteSession({ ...options, createCheckpoint: false });
      await authority.unlock(Buffer.from(password));
      source = await RepositorySession.open(sourcePath, { encryptedRemote: authority, identity });
      const retryStart = proxy.records.length;
      const result = await source.pushInitialBranch();
      assert.equal(result.state, "connected");
      assert.equal(result.verification, "publication");
      if (fault === "none")
        assert.equal(proxy.records.slice(retryStart).filter(payloadRead).length, 0);
      const repeat = await source.pushInitialBranch();
      assert.equal(repeat.push.commits, 0);
      assert.equal((await source.repositoryMetadata()).current_head, commit);
      const initialization = proxy.records.slice(start);
      const peerOptions = {
        ...options,
        createCheckpoint: true,
        checkpointDirectory: privateDirectory("peer-checkpoint"),
        stagingDirectory: privateDirectory("peer-staging"),
        recoveryDirectory: privateDirectory("peer-recovery"),
      };
      peerAuthority = new EncryptedRemoteSession(peerOptions);
      await peerAuthority.unlock(Buffer.from(password));
      const peerPath = join(root, `${name}-peer`);
      peer = await RepositorySession.open(peerPath, { encryptedRemote: peerAuthority, identity });
      await peer.cloneEncryptedRemote({ branch: "main" });
      assert.deepEqual(await readFile(join(peerPath, "payload.bin")), payload);
      const peerDb = new DatabaseSync(join(peerPath, "notes.sqlite"));
      assert.equal(peerDb.prepare("SELECT body FROM notes").get().body, "candidate data");
      if (databaseMiB) {
        const hash = createHash("sha256");
        let count = 0;
        for (const row of peerDb.prepare("SELECT data FROM blocks ORDER BY id").iterate()) {
          hash.update(row.data);
          count++;
        }
        assert.equal(count, databaseMiB);
        assert.equal(hash.digest("hex"), databaseDigest);
      }
      peerDb.close();
      if (fault === "none") {
        const fullStart = proxy.records.length;
        const fullPath = join(root, `${name}-full`);
        assert.equal(
          (await source.pushInitialBranch({ verificationDirectory: fullPath })).verification,
          "full_restore",
        );
        assert.deepEqual(await readFile(join(fullPath, "payload.bin")), payload);
        assert.ok(
          proxy.records.slice(fullStart).some(payloadRead),
          "explicit full restore did not download payload",
        );
      }
      await writeFile(join(peerPath, "roundtrip.txt"), "peer update");
      const updateDb = new DatabaseSync(join(peerPath, "notes.sqlite"));
      updateDb.exec("UPDATE notes SET body='peer update' WHERE id=1");
      updateDb.close();
      await peer.addAll();
      await peer.commit("Peer update");
      await peer.push({ remote: "origin", branch: "main" });
      await source.pull({ remote: "origin", branch: "main" });
      assert.equal(await readFile(join(sourcePath, "roundtrip.txt"), "utf8"), "peer update");
      const checkedDb = new DatabaseSync(join(sourcePath, "notes.sqlite"));
      assert.equal(checkedDb.prepare("SELECT body FROM notes WHERE id=1").get().body, "peer update");
      checkedDb.close();
      await writeFile(join(sourcePath, "roundtrip.txt"), "source reply");
      await source.addAll();
      await source.commit("Source reply");
      await source.push({ remote: "origin", branch: "main" });
      await peer.pull({ remote: "origin", branch: "main" });
      assert.equal(await readFile(join(peerPath, "roundtrip.txt"), "utf8"), "source reply");
      summaries.push({
        fault,
        fallback: fault !== "none",
        injected: proxy.injected,
        defaultPayloadReads: initialization.filter(payloadRead).length,
        initialPayloadReads: firstAttempt.filter(payloadRead).length,
        missingObjectProbes: initialization.filter(
          (row) => row.method === "GET" && payloadPath(row.path) && row.status === 404,
        ).length,
        headerProbeBytes: initialization.filter((row) => row.headerProbe).reduce((sum, row) => sum + row.downloadBytes, 0),
        payloadDownloadBytes: initialization
          .filter(payloadRead)
          .reduce((sum, row) => sum + row.downloadBytes, 0),
        initializationDownloadBytes: initialization.reduce(
          (sum, row) => sum + row.downloadBytes,
          0,
        ),
        initialAttemptDownloadBytes: firstAttempt.reduce((sum, row) => sum + row.downloadBytes, 0),
        multipartRequests: initialization.filter((row) => row.path.includes("/multipart-")).length,
        payloadBytes: payload.length,
        databaseMiB,
        bidirectionalUpdates: true,
        recovered: true,
        independentClone: true,
      });
      console.log(
        JSON.stringify({
          result: "passed",
          scenario: summaries.at(-1),
          nativeSha256: nativeHash,
          sdkVersion: version,
        }),
      );
    } finally {
      await peer?.close();
      await peerAuthority?.disconnect();
      await source?.close();
      await authority?.disconnect();
      await proxy.close();
    }
  }
  try {
    for (const fault of process.env.GRAFT_CANDIDATE_SCENARIOS?.split(",") ?? [
      "none",
      "interrupt-upload",
      "lost-object-response",
      "lost-ref-response",
      "server-crash",
      "client-crash",
    ])
      await scenario(fault);
    console.log(
      JSON.stringify(
        {
          sdk: {
            version,
            nativeSha256: nativeHash,
            unpublishedCandidate: process.env.GRAFT_SDK_RELEASED !== "1",
          },
          serverArchitecture: "see Docker engine",
          scenarios: summaries,
        },
        null,
        2,
      ),
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}
