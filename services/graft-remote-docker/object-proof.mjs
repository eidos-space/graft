import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { join } from "node:path";
import { GraftProtocolError } from "../../packages/graft-remote/dist/index.js";

/** Called only after shared-handler validation and repository admission. */
export async function encryptedObjectProof(storage, repository, path, prefix, requestSignal) {
  if (storage.proofsActive >= 2)
    throw new GraftProtocolError(503, "object_proof_busy", "Retry proof later", {
      "Retry-After": "1",
    });
  storage.proofsActive += 1;
  try {
    const signal = AbortSignal.any([
      AbortSignal.timeout(60_000),
      ...(requestSignal ? [requestSignal] : []),
    ]);
    signal.throwIfAborted();
    const row = storage.db
      .prepare("SELECT value,file,size FROM objects WHERE repository=? AND path=?")
      .get(repository, path);
    if (!row) return null;
    const hash = createHash("sha256").update(prefix);
    let size = 0;
    if (row.file) {
      // Published files are immutable. Open once; the admission lease prevents
      // deletion for the entire read. A renamed/unlinked path cannot switch this fd.
      const stream = storage.blobStore
        ? (await storage.blobStore.get(row.file, undefined, signal, row.size)).body
        : createReadStream(join(storage.blobDirectory, row.file), { signal });
      for await (const chunk of stream) {
        hash.update(chunk);
        size += chunk.length;
      }
      if (size !== row.size)
        throw new GraftProtocolError(
          409,
          "object_length_mismatch",
          "Stored ciphertext length changed",
        );
    } else {
      hash.update(row.value);
      size = row.value.length;
    }
    signal.throwIfAborted();
    return { size, sha256: hash.digest("hex") };
  } finally {
    storage.proofsActive -= 1;
  }
}
