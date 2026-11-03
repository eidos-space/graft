// Optional source-client compatibility test. Run with Bun; no Lite profile is opened.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
const { origin, token, clientPath } = JSON.parse(readFileSync(0, "utf8"));
const { CustomSyncClient } = await import(clientPath);
const client = new CustomSyncClient(origin, fetch, true);
await client.discover();
await assert.rejects(client.listRepositories("incorrect-access-key"));
const result = await client.provisionRepository("lite-source-test", "Lite 测试", token);
assert.equal(result.created, true);
await client.verifyRepository(result.remoteUrl, token, result.encryption);
await client.renameRepository("lite-source-test", "Renamed 测试", token);
const listed = await client.listRepositories(token);
assert.ok(
  listed.repositories.some(
    (row) => row.name === "lite-source-test" && row.displayName === "Renamed 测试",
  ),
);
console.log(
  "PASS: actual Lite CustomSyncClient source discovery, auth, provisioning, descriptor and listing",
);
