import { DatabaseSync } from "node:sqlite";
import { randomUUID } from "node:crypto";
import { mkdirSync, createReadStream, openSync, fsyncSync, closeSync } from "node:fs";
import { unlink } from "node:fs/promises";
import { dirname, join } from "node:path";
import { Readable } from "node:stream";
import { multipartBackend } from "./multipart.mjs";
import { encryptedObjectProof } from "./object-proof.mjs";
import { GraftProtocolError } from "../../packages/graft-remote/dist/index.js";

export const MAX_BYTES = 64 * 1024 * 1024;

export async function readBytes(body, maximum = MAX_BYTES) {
  if (!(body instanceof ReadableStream)) {
    if (body.byteLength > maximum)
      throw new GraftProtocolError(413, "request_too_large", "Request exceeds byte limit");
    return body;
  }
  const chunks = [];
  let size = 0;
  for await (const chunk of body) {
    size += chunk.byteLength;
    if (size > maximum)
      throw new GraftProtocolError(413, "request_too_large", "Request exceeds byte limit");
    chunks.push(chunk);
  }
  return Buffer.concat(chunks, size);
}

/** One local SQLite database owns directory, identities and object publication. */
export class Storage {
  constructor(filename) {
    this.active = new Map();
    this.proofsActive = 0;
    this.completing = new Set();
    this.blobDirectory = join(dirname(filename), "blobs");
    mkdirSync(this.blobDirectory, { recursive: true, mode: 0o700 });
    const parent = openSync(dirname(filename), "r");
    try {
      fsyncSync(parent);
    } finally {
      closeSync(parent);
    }
    this.db = new DatabaseSync(filename);
    const version = this.db.prepare("PRAGMA user_version").get().user_version;
    if (![0, 1, 2, 3, 4].includes(version)) {
      this.db.close();
      throw new Error("Unsupported storage version");
    }
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
      PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;`);
    this.db.exec(`BEGIN IMMEDIATE;
      CREATE TABLE IF NOT EXISTS repositories (
        name TEXT PRIMARY KEY, display_name TEXT NOT NULL, created_at INTEGER NOT NULL,
        uuid TEXT UNIQUE
      );
      CREATE TABLE IF NOT EXISTS objects (
        repository TEXT NOT NULL REFERENCES repositories(name),
        path TEXT COLLATE BINARY NOT NULL, value BLOB NOT NULL,
        PRIMARY KEY(repository, path)
      ) WITHOUT ROWID;
      COMMIT;`);
    if (version < 2)
      this.db.exec(`BEGIN IMMEDIATE;
      ALTER TABLE repositories ADD COLUMN status TEXT NOT NULL DEFAULT 'active'
        CHECK(status IN ('active','deleting','deleted'));
      PRAGMA user_version=2; COMMIT;`);
    if (version < 3)
      this.db.exec(`BEGIN IMMEDIATE;
      ALTER TABLE objects ADD COLUMN file TEXT;
      ALTER TABLE objects ADD COLUMN size INTEGER;
      CREATE TABLE uploads(id TEXT PRIMARY KEY, repository TEXT NOT NULL REFERENCES repositories(name),
        path TEXT NOT NULL, total INTEGER NOT NULL, part_bytes INTEGER NOT NULL,
        identity TEXT NOT NULL, completing INTEGER NOT NULL DEFAULT 0, UNIQUE(repository,path));
      CREATE TABLE parts(upload TEXT REFERENCES uploads(id) ON DELETE CASCADE, number INTEGER,
        value BLOB NOT NULL, sha TEXT NOT NULL, PRIMARY KEY(upload,number)) WITHOUT ROWID;
      CREATE TABLE blob_files(file TEXT PRIMARY KEY, repository TEXT NOT NULL REFERENCES repositories(name));
      PRAGMA user_version=3; COMMIT;`);
    if (version < 4) {
      this.db.exec(
        "BEGIN IMMEDIATE; ALTER TABLE uploads ADD COLUMN updated INTEGER NOT NULL DEFAULT 0; PRAGMA user_version=4; COMMIT;",
      );
      this.db.prepare("UPDATE uploads SET updated=? WHERE updated=0").run(Date.now());
    }
    this.cleaning = false;
    this.cleanupTimer = setInterval(async () => {
      if (this.cleaning || this.closed) return;
      this.cleaning = true;
      try {
        for (const row of this.db
          .prepare("SELECT name FROM repositories WHERE status='deleting'")
          .all()) {
          try {
            await this.cleanupStep(row.name);
          } catch {
            console.error("Repository cleanup deferred");
          }
        }
        if (this.closed) return;
        const expired = this.db
          .prepare("SELECT id,repository FROM uploads WHERE updated<? LIMIT 16")
          .all(Date.now() - 7 * 86400_000);
        for (const row of expired) {
          if (!this.active.has(row.repository) && !this.completing.has(row.id))
            this.db.prepare("DELETE FROM uploads WHERE id=?").run(row.id);
        }
        const garbage = this.db
          .prepare(`SELECT file FROM blob_files WHERE file NOT IN
        (SELECT file FROM objects WHERE file IS NOT NULL) AND file NOT IN (SELECT id FROM uploads) LIMIT 16`)
          .all();
        for (const row of garbage) {
          try {
            await this.collectBlob(row.file);
          } catch {}
        }
      } finally {
        this.cleaning = false;
      }
    }, 1000);
    this.cleanupTimer.unref();
  }

  close() {
    this.closed = true;
    clearInterval(this.cleanupTimer);
    this.db.close();
  }
  health() {
    this.db.prepare("SELECT 1 FROM repositories LIMIT 1").get();
  }
  lookup(name) {
    return this.db.prepare("SELECT * FROM repositories WHERE name=?").get(name);
  }
  list() {
    return this.db
      .prepare("SELECT * FROM repositories WHERE status!='deleted' ORDER BY created_at DESC, name")
      .all();
  }
  provision(name, displayName, encrypted) {
    // A single statement publishes the immutable identity, even across concurrent requests.
    const created =
      this.db
        .prepare(
          "INSERT OR IGNORE INTO repositories(name,display_name,created_at,uuid) VALUES (?, ?, ?, ?)",
        )
        .run(name, displayName, Date.now(), encrypted ? randomUUID() : null).changes === 1;
    const row = this.lookup(name);
    if (row.status !== "active")
      throw new GraftProtocolError(
        409,
        "repository_name_reserved",
        "Repository technical names cannot be reused",
      );
    if ((row.uuid !== null) !== encrypted)
      throw new GraftProtocolError(
        409,
        "profile_conflict",
        "Repository encryption profile cannot change",
      );
    return { created, row };
  }
  rename(name, displayName) {
    this.assertActive(name);
    this.db.prepare("UPDATE repositories SET display_name=? WHERE name=?").run(displayName, name);
    return this.lookup(name);
  }

  assertActive(name) {
    const row = this.lookup(name);
    if (!row) throw new GraftProtocolError(404, "repository_not_found", "Repository not found");
    if (row.status !== "active")
      throw new GraftProtocolError(410, "repository_deleted", "Repository is deleting or deleted");
    return row;
  }
  admit(name) {
    const row = this.assertActive(name);
    this.active.set(name, (this.active.get(name) ?? 0) + 1);
    let released = false;
    return {
      row,
      release: () => {
        if (released) return;
        released = true;
        const remaining = this.active.get(name) - 1;
        if (remaining === 0) this.active.delete(name);
        else this.active.set(name, remaining);
      },
    };
  }
  requestDelete(name) {
    this.db
      .prepare("UPDATE repositories SET status='deleting' WHERE name=? AND status='active'")
      .run(name);
    return this.lookup(name);
  }
  async cleanupStep(name) {
    if (this.active.has(name) || this.lookup(name)?.status !== "deleting") return;
    this.db.exec("BEGIN IMMEDIATE");
    try {
      this.db.prepare("DELETE FROM uploads WHERE repository=?").run(name);
      this.db
        .prepare(
          "DELETE FROM objects WHERE repository=? AND path IN (SELECT path FROM objects WHERE repository=? LIMIT 128)",
        )
        .run(name, name);
      if (
        !this.db.prepare("SELECT 1 FROM objects WHERE repository=? LIMIT 1").get(name) &&
        !this.db.prepare("SELECT 1 FROM blob_files WHERE repository=? LIMIT 1").get(name)
      ) {
        this.db
          .prepare("UPDATE repositories SET status='deleted' WHERE name=? AND status='deleting'")
          .run(name);
      }
      this.db.exec("COMMIT");
    } catch (error) {
      this.db.exec("ROLLBACK");
      throw error;
    }
    for (const row of this.db
      .prepare("SELECT file FROM blob_files WHERE repository=? LIMIT 16")
      .all(name))
      await this.collectBlob(row.file);
  }
  async collectBlob(file) {
    if (
      this.completing.has(file) ||
      this.db.prepare("SELECT 1 FROM objects WHERE file=?").get(file) ||
      this.db.prepare("SELECT 1 FROM uploads WHERE id=?").get(file)
    )
      return;
    try {
      await unlink(join(this.blobDirectory, file));
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    const directory = openSync(this.blobDirectory, "r");
    try {
      fsyncSync(directory);
    } finally {
      closeSync(directory);
    }
    if (!this.closed) this.db.prepare("DELETE FROM blob_files WHERE file=?").run(file);
  }

  backend(row, { signal } = {}) {
    const db = this.db;
    const storageDirectory = this.blobDirectory;
    const name = row.name;
    const active = () => this.assertActive(name);
    return {
      ...(row.uuid === null
        ? {}
        : {
            encryption: descriptor(row.uuid),
            encryptedObjectProof: (path, prefix) =>
              encryptedObjectProof(this, row.name, path, prefix, signal),
          }),
      multipart: multipartBackend(this, row),
      head(path) {
        const found = db
          .prepare(
            "SELECT COALESCE(size,length(value)) AS size FROM objects WHERE repository=? AND path=?",
          )
          .get(name, path);
        return found ?? null;
      },
      get(path, range) {
        const found = db
          .prepare("SELECT value,file,size FROM objects WHERE repository=? AND path=?")
          .get(name, path);
        if (!found) return null;
        if (found.file) {
          const stream = createReadStream(
            join(storageDirectory, found.file),
            range ? { start: range.start, end: range.end } : {},
          );
          return { size: found.size, body: Readable.toWeb(stream) };
        }
        const body = range ? found.value.subarray(range.start, range.end + 1) : found.value;
        return { size: body.length, body };
      },
      put(path, value) {
        active();
        db.prepare(
          "INSERT INTO objects(repository,path,value) VALUES (?, ?, ?) ON CONFLICT(repository,path) DO UPDATE SET value=excluded.value,file=NULL,size=NULL",
        ).run(name, path, value);
      },
      delete(path) {
        active();
        db.prepare("DELETE FROM objects WHERE repository=? AND path=?").run(name, path);
      },
      async putIfAbsent(path, body) {
        const value = await readBytes(body);
        active();
        return (
          db
            .prepare("INSERT OR IGNORE INTO objects(repository,path,value) VALUES (?, ?, ?)")
            .run(name, path, value).changes === 1
        );
      },
      compareAndSwap(path, expected, replacement) {
        active();
        if (expected === undefined)
          return (
            db
              .prepare("INSERT OR IGNORE INTO objects(repository,path,value) VALUES (?, ?, ?)")
              .run(name, path, replacement).changes === 1
          );
        return (
          db
            .prepare("UPDATE objects SET value=? WHERE repository=? AND path=? AND value=?")
            .run(replacement, name, path, expected).changes === 1
        );
      },
      compareAndDelete(path, expected) {
        active();
        if (expected === undefined)
          return !db.prepare("SELECT 1 FROM objects WHERE repository=? AND path=?").get(name, path);
        return (
          db
            .prepare("DELETE FROM objects WHERE repository=? AND path=? AND value=?")
            .run(name, path, expected).changes === 1
        );
      },
      list({ prefix, after, limit }) {
        const rows = db
          .prepare(`SELECT path, COALESCE(size,length(value)) AS size FROM objects
          WHERE repository=? AND path>=? AND path<? AND path>? ORDER BY path LIMIT ?`)
          .all(name, prefix, `${prefix}\u{10ffff}`, after ?? "", limit + 1);
        const entries = rows.slice(0, limit);
        return { paths: entries.map((row) => row.path), entries, hasMore: rows.length > limit };
      },
    };
  }
}

export function descriptor(uuid) {
  return uuid === null ? null : { profile: "encrypted-remote-v1", version: 1, suite: 1, uuid };
}
