"use strict";

// Mandatory release smoke: never silently skip when the native feature is absent.
// The optional argument selects a fresh npm install for post-publication proof.
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const os = require("node:os");
const path = require("node:path");
const { EncryptedRemoteSession, RepositorySession, sdkVersion } = require(
  process.argv[2] ? path.resolve(process.argv[2]) : "..",
);

async function main() {
  assert.equal(typeof EncryptedRemoteSession, "function");
  const directory = await fs.mkdtemp(path.join(os.tmpdir(), "graft-release-e2ee-"));
  let authority;
  let repository;
  try {
    const paths = Object.fromEntries(
      await Promise.all(
        ["staging", "checkpoint", "recovery", "repository"].map(async (name) => {
          const target = path.join(directory, name);
          await fs.mkdir(target, { mode: 0o700 });
          return [name, target];
        }),
      ),
    );
    const options = {
      remoteName: "origin",
      url: "https://release-check.invalid/namespace/repository",
      uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f",
      authenticationDomain: "https://release-check.invalid",
      accountSubject: "synthetic-account",
      bearerToken: "synthetic-token",
      checkpointDirectory: paths.checkpoint,
      recoveryDirectory: paths.recovery,
      stagingDirectory: paths.staging,
      createCheckpoint: true,
    };
    // This enters native parsing before touching disk. A plain addon rejects with
    // UNSUPPORTED_ENCRYPTED_PROFILE, which must fail this release gate.
    assert.throws(() => new EncryptedRemoteSession({ ...options, uuid: "invalid" }), {
      code: "GRAFT_SDK_E2EE_INVALID_SESSION_BINDING",
    });
    if (process.platform === "win32") {
      assert.throws(() => new EncryptedRemoteSession(options), {
        code: "GRAFT_SDK_E2EE_UNSUPPORTED_ENCRYPTED_HOST",
      });
      for (const name of ["checkpoint", "recovery", "staging"])
        assert.deepEqual(await fs.readdir(paths[name]), []);
      console.log(
        `Verified Graft ${sdkVersion()} native E2EE API and explicit unsupported Windows storage boundary`,
      );
      return;
    }
    authority = new EncryptedRemoteSession(options);
    assert.equal(authority.state, "locked");
    authority.pause(true);
    assert.equal(authority.paused, true);
    authority.pause(false);
    assert.equal(authority.paused, false);
    assert.throws(
      () =>
        authority.refreshBearerToken(
          options.authenticationDomain,
          "different-account",
          "replacement-token",
        ),
      { code: "GRAFT_SDK_E2EE_IDENTITY_CHANGED" },
    );
    const invalidPassword = Buffer.from([0xff]);
    await assert.rejects(authority.unlock(invalidPassword), {
      code: "GRAFT_SDK_E2EE_INVALID_PASSWORD",
    });
    assert.ok(invalidPassword.every((byte) => byte === 0));
    assert.equal(authority.state, "locked");
    repository = await RepositorySession.open(paths.repository, { encryptedRemote: authority });
    await repository.init();
    await repository.addEncryptedRemote();
    assert.equal((await repository.listRemotes()).remotes[0].kind, "encrypted_http");
    await assert.rejects(repository.fetch({ remote: "origin", branch: "main" }), {
      code: "GRAFT_SDK_E2EE_LOCKED",
    });
    await repository.close();
    repository = undefined;
    await authority.disconnect();
    authority = new EncryptedRemoteSession({ ...options, createCheckpoint: false });
    assert.equal(authority.state, "locked");
    await authority.lock();
    console.log(
      `Verified Graft ${sdkVersion()} native E2EE enrollment, reopening, isolation, and locked access`,
    );
  } finally {
    await repository?.close();
    await authority?.disconnect();
    await fs.rm(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
