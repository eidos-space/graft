import { createHash, randomUUID } from "node:crypto";
import { open, unlink } from "node:fs/promises";
import { join } from "node:path";
import { GraftProtocolError } from "../../packages/graft-remote/dist/index.js";
import { readBytes } from "./storage.mjs";

const conflict = () =>
  new GraftProtocolError(
    409,
    "upload_session_changed",
    "Multipart session changed or is incomplete",
  );
const invalid = () =>
  new GraftProtocolError(
    400,
    "invalid_encrypted_ciphertext",
    "Ciphertext does not match upload identity",
  );

/** Parts are durable SQLite rows; complete streams one part at a time to disk. */
export function multipartBackend(storage, repository, signal) {
  const db = storage.db;
  const name = repository.name;
  const active = () => storage.assertActive(name);
  const lookup = (path, id) => {
    const row = db.prepare("SELECT * FROM uploads WHERE repository=? AND path=?").get(name, path);
    if (!row || row.id !== id) throw conflict();
    return row;
  };
  return {
    supportsEncryption: true,
    start(path, totalBytes, partBytes, encryption) {
      active();
      if (db.prepare("SELECT 1 FROM objects WHERE repository=? AND path=?").get(name, path))
        return null;
      if (
        !Number.isSafeInteger(totalBytes) ||
        totalBytes < 1 ||
        partBytes < 1 ||
        partBytes > 64 * 1024 * 1024 ||
        Math.ceil(totalBytes / partBytes) > 10000
      )
        throw conflict();
      if (
        repository.uuid !== null &&
        (!encryption ||
          encryption.uuid !== repository.uuid ||
          !/^[0-9a-f]{64}$/.test(encryption.uploadId) ||
          !/^[0-9a-f]{64}$/.test(encryption.ciphertextSha256))
      )
        throw invalid();
      const identity = JSON.stringify(encryption ?? null);
      db.prepare(
        "INSERT OR IGNORE INTO uploads(id,repository,path,total,part_bytes,identity,updated) VALUES(?,?,?,?,?,?,?)",
      ).run(randomUUID(), name, path, totalBytes, partBytes, identity, Date.now());
      const row = db.prepare("SELECT * FROM uploads WHERE repository=? AND path=?").get(name, path);
      if (row.total !== totalBytes || row.part_bytes !== partBytes || row.identity !== identity)
        throw conflict();
      db.prepare("UPDATE uploads SET updated=? WHERE id=?").run(Date.now(), row.id);
      const uploadedParts = db
        .prepare(
          "SELECT number AS partNumber, length(value) AS bytes FROM parts WHERE upload=? ORDER BY number",
        )
        .all(row.id);
      return {
        uploadId: row.id,
        totalBytes,
        partBytes,
        uploadedParts,
        ...(encryption ? { encryption } : {}),
      };
    },
    async uploadPart(path, id, number, body, length) {
      const row = lookup(path, id);
      const expected = Math.min(row.part_bytes, row.total - (number - 1) * row.part_bytes);
      if (
        !Number.isSafeInteger(number) ||
        number < 1 ||
        number > Math.ceil(row.total / row.part_bytes) ||
        length !== expected
      )
        throw conflict();
      const bytes = await readBytes(body, expected);
      if (bytes.length !== expected) throw conflict();
      active();
      if (lookup(path, id).completing) throw conflict();
      const sha = createHash("sha256").update(bytes).digest("hex");
      db.prepare("INSERT OR IGNORE INTO parts(upload,number,value,sha) VALUES(?,?,?,?)").run(
        id,
        number,
        bytes,
        sha,
      );
      db.prepare("UPDATE uploads SET updated=? WHERE id=?").run(Date.now(), id);
      if (
        db.prepare("SELECT sha FROM parts WHERE upload=? AND number=?").get(id, number).sha !== sha
      )
        throw conflict();
    },
    async complete(path, id) {
      active();
      if (db.prepare("SELECT 1 FROM objects WHERE repository=? AND path=?").get(name, path))
        return false;
      const row = lookup(path, id);
      if (storage.completing.has(id)) throw conflict();
      const count = Math.ceil(row.total / row.part_bytes);
      const parts = db
        .prepare("SELECT COUNT(*) AS count, SUM(length(value)) AS size FROM parts WHERE upload=?")
        .get(id);
      if (parts.count !== count || parts.size !== row.total) throw conflict();
      db.prepare("UPDATE uploads SET completing=1,updated=? WHERE id=?").run(Date.now(), id);
      // Register the file before creating it, so deletion can reclaim crash leftovers.
      db.prepare("INSERT OR IGNORE INTO blob_files(file,repository) VALUES(?,?)").run(id, name);
      storage.completing.add(id);
      let handle;
      try {
        handle = await open(join(storage.blobDirectory, id), "w", 0o600);
        const hash = createHash("sha256");
        let prefix;
        for (let number = 1; number <= count; number++) {
          const part = db
            .prepare("SELECT value FROM parts WHERE upload=? AND number=?")
            .get(id, number);
          if (!part) throw conflict();
          const bytes = Buffer.from(part.value);
          if (number === 1) prefix = bytes.subarray(0, 72);
          hash.update(bytes);
          // FileHandle.write can make short writes; writeFile handles the full buffer.
          await handle.writeFile(bytes);
        }
        const sha = hash.digest("hex");
        const identity = JSON.parse(row.identity);
        if (
          identity &&
          (sha !== identity.ciphertextSha256 ||
            prefix.length !== 72 ||
            prefix.subarray(0, 8).toString() !== "GRAFTEO1" ||
            prefix.readUInt16BE(8) !== 1 ||
            prefix.readUInt16BE(10) !== 1 ||
            prefix.subarray(12, 44).toString("hex") !== identity.uploadId ||
            prefix.readUInt32BE(68) < 16 ||
            prefix.readUInt32BE(68) > 16384 ||
            row.total < 72 + prefix.readUInt32BE(68) + 16)
        )
          throw invalid();
        await handle.sync();
        await handle.close();
        handle = undefined;
        const directory = await open(storage.blobDirectory, "r");
        try {
          await directory.sync();
        } finally {
          await directory.close();
        }
        await storage.persistBlob(id, row.total, signal);
        active();
        lookup(path, id); // Abort/delete fences completion after async file I/O.
        db.exec("BEGIN IMMEDIATE");
        try {
          const created =
            db
              .prepare(
                "INSERT OR IGNORE INTO objects(repository,path,value,file,size) VALUES(?,?,?, ?,?)",
              )
              .run(name, path, Buffer.alloc(0), id, row.total).changes === 1;
          db.prepare("DELETE FROM uploads WHERE id=?").run(id);
          db.exec("COMMIT");
          return created;
        } catch (error) {
          db.exec("ROLLBACK");
          throw error;
        }
      } finally {
        await handle?.close();
        storage.completing.delete(id);
      }
    },
    async abort(path, id) {
      active();
      db.prepare("DELETE FROM uploads WHERE repository=? AND path=? AND id=?").run(name, path, id);
      // Leave registered files to the garbage collector if an assembler is running.
      if (!storage.completing.has(id)) await storage.collectBlob(id);
    },
  };
}
