// Candidate-only subprocess. Configuration arrives over stdin, never argv/logs.
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
const input = JSON.parse(readFileSync(0, "utf8"));
const { RepositorySession, EncryptedRemoteSession } = createRequire(import.meta.url)(
  input.modulePath,
);
const authority = new EncryptedRemoteSession(input.options);
await authority.unlock(Buffer.from(input.password));
const source = await RepositorySession.open(input.sourcePath, { encryptedRemote: authority });
try {
  const result = await source.pushInitialBranch();
  console.log(JSON.stringify(result));
} finally {
  await source.close();
  await authority.disconnect();
}
