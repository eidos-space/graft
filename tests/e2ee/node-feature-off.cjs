"use strict";
const assert = require("node:assert/strict");
const { mkdtempSync, rmSync } = require("node:fs");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const { EncryptedRemoteSession, RepositorySession } = require("../../packages/graft-sdk");
const native = require(process.env.GRAFT_SDK_NATIVE_PATH);
assert.equal(native.EncryptedRemoteSession, undefined);
assert.equal(native.RepositorySession.withEncryptedRemote, undefined);
assert.throws(() => new EncryptedRemoteSession({}), { code: "GRAFT_SDK_E2EE_UNSUPPORTED_ENCRYPTED_PROFILE" });
const directory = mkdtempSync(join(tmpdir(), "graft-node-feature-off-"));
(async () => {
  const session = await RepositorySession.open(directory);
  try {
    await session.init();
    assert.equal(session.lifecycle, "open");
    await assert.rejects(session.cloneEncryptedRemote(), { code: "GRAFT_SDK_INVALID_ARGUMENT" });
    for (const method of ["prepareEncryptedMigration", "resumeEncryptedMigration", "verifyEncryptedMigration", "cutoverEncryptedMigration"]) {
      assert.equal(native.RepositorySession.prototype[method], undefined);
      const password = Buffer.from("feature-off migration password");
      assert.throws(() => session[method](password), { code: "GRAFT_SDK_E2EE_UNSUPPORTED_ENCRYPTED_PROFILE" });
      assert.ok(password.every((value) => value === 0));
    }
  } finally {
    await session.close();
    rmSync(directory, { recursive: true, force: true });
  }
  console.log("Default Node addon rejects E2EE management and retains repository ABI");
})().catch((error) => { console.error(error); process.exitCode = 1; });
