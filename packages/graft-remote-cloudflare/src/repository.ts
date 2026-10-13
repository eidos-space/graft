import { DurableObject } from "cloudflare:workers";
import {
  type EncryptedRoutingHint,
  type GraftEncryptionDescriptor,
  type GraftMultipartEncryptionIdentity,
} from "@eidos.space/graft-remote";

export type RepositoryProfile = { profile: "plain" } | GraftEncryptionDescriptor;
export interface EncryptedObjectRecord {
  path: string;
  key: string;
  size: number;
}
export interface EncryptedUploadState {
  path: string;
  key: string;
  uploadId: string;
  totalBytes: number;
  partBytes: number;
  encryption: GraftMultipartEncryptionIdentity;
  completing: boolean;
  failed?: boolean;
  uploadedParts: Array<{ partNumber: number; etag: string; bytes: number }>;
}
interface JsonRow {
  [key: string]: SqlStorageValue;
  value: string;
}
interface RoutingCommitRow {
  [key: string]: SqlStorageValue;
  parents: string;
  conflict: number;
}
interface RoutingHintRow {
  [key: string]: SqlStorageValue;
  key: string;
  pack_key: string;
  index_key: string;
}

interface MetadataRow {
  [key: string]: SqlStorageValue;
  value: ArrayBuffer;
}

interface PathRow {
  [key: string]: SqlStorageValue;
  path: string;
}

interface ChangeRow {
  [key: string]: SqlStorageValue;
  changed: number;
}

interface MultipartUploadRow {
  [key: string]: SqlStorageValue;
  upload_id: string;
  total_bytes: number;
  part_bytes: number;
}

interface MultipartPartRow {
  [key: string]: SqlStorageValue;
  part_number: number;
  etag: string;
  bytes: number;
}

export interface MetadataListResult {
  paths: string[];
  hasMore: boolean;
}

export interface MultipartUploadState {
  uploadId: string;
  totalBytes: number;
  partBytes: number;
  uploadedParts: Array<{ partNumber: number; etag: string; bytes: number }>;
}

export class RepositoryDurableObject extends DurableObject {
  constructor(ctx: DurableObjectState, env: Cloudflare.Env) {
    super(ctx, env);
    this.ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS repository_profile (id INTEGER PRIMARY KEY CHECK(id = 1), value TEXT NOT NULL)",
    );
    this.ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS encrypted_objects (path TEXT PRIMARY KEY, value TEXT NOT NULL)",
    );
    this.ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS encrypted_uploads (path TEXT PRIMARY KEY, value TEXT NOT NULL)",
    );
    this.ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS encrypted_upload_parts (path TEXT NOT NULL, upload_id TEXT NOT NULL, part_number INTEGER NOT NULL, etag TEXT NOT NULL, bytes INTEGER NOT NULL, PRIMARY KEY(path, upload_id, part_number))",
    );
    this.ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS encrypted_upload_writes (path TEXT NOT NULL, upload_id TEXT NOT NULL, part_number INTEGER NOT NULL, PRIMARY KEY(path, upload_id, part_number))",
    );
    this.ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS encrypted_route_hints (key TEXT PRIMARY KEY, pack_key TEXT NOT NULL, index_key TEXT NOT NULL)",
    );
    this.ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS encrypted_route_commits (token TEXT PRIMARY KEY, parents TEXT NOT NULL, conflict INTEGER NOT NULL)",
    );
    this.ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS encrypted_route_members (token TEXT NOT NULL, hint_key TEXT NOT NULL, PRIMARY KEY(token, hint_key))",
    );
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS metadata (
        path TEXT PRIMARY KEY,
        value BLOB NOT NULL
      )
    `);
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS multipart_uploads (
        path TEXT PRIMARY KEY,
        upload_id TEXT NOT NULL UNIQUE,
        total_bytes INTEGER NOT NULL CHECK (total_bytes > 0),
        part_bytes INTEGER NOT NULL CHECK (part_bytes > 0),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
      )
    `);
    this.ctx.storage.sql.exec(`
      CREATE TABLE IF NOT EXISTS multipart_parts (
        path TEXT NOT NULL,
        upload_id TEXT NOT NULL,
        part_number INTEGER NOT NULL CHECK (part_number > 0),
        etag TEXT NOT NULL,
        bytes INTEGER NOT NULL CHECK (bytes > 0),
        PRIMARY KEY (path, part_number)
      )
    `);
  }

  async getProfile(): Promise<RepositoryProfile | null> {
    const row = this.ctx.storage.sql
      .exec<JsonRow>("SELECT value FROM repository_profile WHERE id = 1")
      .toArray()[0];
    return row === undefined ? null : (JSON.parse(row.value) as RepositoryProfile);
  }

  async registerProfile(profile: RepositoryProfile): Promise<boolean> {
    // SQL work is synchronous: competing plain/encrypted opens have one winner.
    const encoded = JSON.stringify(profile);
    const previous = this.ctx.storage.sql
      .exec<JsonRow>("SELECT value FROM repository_profile WHERE id = 1")
      .toArray()[0];
    if (previous !== undefined) {
      return previous.value === encoded;
    }
    if (
      profile.profile !== "plain" &&
      this.ctx.storage.sql.exec("SELECT 1 FROM metadata LIMIT 1").toArray().length > 0
    )
      return false;
    this.ctx.storage.sql.exec("INSERT INTO repository_profile(id, value) VALUES (1, ?)", encoded);
    return true;
  }

  async getEncryptedObject(path: string): Promise<EncryptedObjectRecord | null> {
    const row = this.ctx.storage.sql
      .exec<JsonRow>("SELECT value FROM encrypted_objects WHERE path = ?", path)
      .toArray()[0];
    return row === undefined ? null : (JSON.parse(row.value) as EncryptedObjectRecord);
  }

  async publishEncryptedObject(
    record: EncryptedObjectRecord,
    hint?: EncryptedRoutingHint,
  ): Promise<boolean> {
    return this.ctx.storage.transactionSync(() => {
      const created =
        this.ctx.storage.sql
          .exec<ChangeRow>(
            "INSERT OR IGNORE INTO encrypted_objects(path, value) VALUES (?, ?) RETURNING 1 AS changed",
            record.path,
            JSON.stringify(record),
          )
          .toArray().length === 1;
      if (created && hint !== undefined) {
        if (hint.key !== record.path) throw new RangeError("Invalid derived hint identity");
        this.indexRoutingHint(hint);
      }
      return created;
    });
  }

  async resetEncryptedRouting(): Promise<void> {
    this.ctx.storage.transactionSync(() => {
      this.ctx.storage.sql.exec("DELETE FROM encrypted_route_members");
      this.ctx.storage.sql.exec("DELETE FROM encrypted_route_commits");
      this.ctx.storage.sql.exec("DELETE FROM encrypted_route_hints");
    });
  }

  async restoreEncryptedRouting(
    record: EncryptedObjectRecord,
    hint: EncryptedRoutingHint,
  ): Promise<boolean> {
    return this.ctx.storage.transactionSync(() => {
      const stored = this.ctx.storage.sql
        .exec<JsonRow>("SELECT value FROM encrypted_objects WHERE path = ?", record.path)
        .toArray()[0];
      if (
        stored === undefined ||
        (JSON.parse(stored.value) as EncryptedObjectRecord).key !== record.key ||
        hint.key !== record.path
      )
        return false;
      this.indexRoutingHint(hint);
      return true;
    });
  }

  private indexRoutingHint(hint: EncryptedRoutingHint): void {
    if (hint.commits.length > 4096) throw new RangeError("Invalid derived hint identity");
    const added = this.ctx.storage.sql
      .exec<ChangeRow>(
        "INSERT OR IGNORE INTO encrypted_route_hints(key, pack_key, index_key) VALUES (?, ?, ?) RETURNING 1 AS changed",
        hint.key,
        hint.pack,
        hint.index,
      )
      .toArray().length;
    if (added === 0) return;
    for (const commit of hint.commits) {
      if (commit.parents.length > 32) throw new RangeError("Invalid derived hint parents");
      this.ctx.storage.sql.exec(
        "INSERT INTO encrypted_route_commits(token, parents, conflict) VALUES (?, ?, 0) ON CONFLICT(token) DO UPDATE SET conflict = CASE WHEN encrypted_route_commits.parents <> excluded.parents THEN 1 ELSE encrypted_route_commits.conflict END",
        commit.token,
        JSON.stringify(commit.parents),
      );
      this.ctx.storage.sql.exec(
        "INSERT INTO encrypted_route_members(token, hint_key) VALUES (?, ?)",
        commit.token,
        hint.key,
      );
    }
  }

  /** Derived SQL index is updated in the same transaction as immutable hint visibility. */
  async selectEncryptedHints(
    target: string,
    have: string | null,
  ): Promise<Array<Pick<EncryptedRoutingHint, "key" | "pack" | "index">> | null> {
    const states = new Map<string, 1 | 2>();
    const selected = new Map<string, Pick<EncryptedRoutingHint, "key" | "pack" | "index">>();
    const pending = [{ token: target, exit: false }];
    while (pending.length !== 0) {
      const { token, exit } = pending.pop()!;
      if (token === have) continue;
      if (exit) {
        states.set(token, 2);
        continue;
      }
      const state = states.get(token);
      if (state === 2) continue;
      if (state === 1 || states.size >= 100_000) return null;
      const commit = this.ctx.storage.sql
        .exec<RoutingCommitRow>(
          "SELECT parents, conflict FROM encrypted_route_commits WHERE token = ?",
          token,
        )
        .toArray()[0];
      if (commit === undefined || commit.conflict !== 0) return null;
      const hint = this.ctx.storage.sql
        .exec<RoutingHintRow>(
          "SELECT h.key, h.pack_key, h.index_key FROM encrypted_route_members m JOIN encrypted_route_hints h ON h.key = m.hint_key WHERE m.token = ? ORDER BY m.hint_key LIMIT 1",
          token,
        )
        .toArray()[0];
      if (hint === undefined) return null;
      selected.set(hint.key, { key: hint.key, pack: hint.pack_key, index: hint.index_key });
      if (selected.size > 128) return null;
      const parents = JSON.parse(commit.parents) as string[];
      if (pending.length + parents.length + 1 > 100_000) return null;
      states.set(token, 1);
      pending.push({ token, exit: true });
      for (let index = parents.length - 1; index >= 0; index--)
        pending.push({ token: parents[index]!, exit: false });
    }
    return [...selected.values()].sort((a, b) => (a.key < b.key ? -1 : a.key > b.key ? 1 : 0));
  }

  async listEncryptedObjects(
    prefix: string,
    after: string,
    limit: number,
  ): Promise<EncryptedObjectRecord[]> {
    return this.ctx.storage.sql
      .exec<JsonRow>(
        "SELECT value FROM encrypted_objects WHERE substr(path, 1, ?) = ? AND path COLLATE BINARY > ? COLLATE BINARY ORDER BY path COLLATE BINARY LIMIT ?",
        prefix.length,
        prefix,
        after,
        limit,
      )
      .toArray()
      .map((row) => JSON.parse(row.value) as EncryptedObjectRecord);
  }

  async getEncryptedUpload(path: string): Promise<EncryptedUploadState | null> {
    return this.readEncryptedUpload(path);
  }

  async createEncryptedUpload(state: EncryptedUploadState): Promise<boolean> {
    return (
      this.ctx.storage.sql
        .exec<ChangeRow>(
          "INSERT OR IGNORE INTO encrypted_uploads(path, value) VALUES (?, ?) RETURNING 1 AS changed",
          state.path,
          JSON.stringify(state),
        )
        .toArray().length === 1
    );
  }

  async recordEncryptedPart(
    path: string,
    uploadId: string,
    part: EncryptedUploadState["uploadedParts"][number],
  ): Promise<boolean> {
    return this.ctx.storage.transactionSync(() =>
      this.recordEncryptedPartSync(path, uploadId, part),
    );
  }

  private recordEncryptedPartSync(
    path: string,
    uploadId: string,
    part: EncryptedUploadState["uploadedParts"][number],
  ): boolean {
    const state = this.readEncryptedUpload(path, false);
    if (state === null || state.uploadId !== uploadId || state.completing) return false;
    if (
      !Number.isSafeInteger(part.partNumber) ||
      part.partNumber < 1 ||
      part.partNumber > Math.ceil(state.totalBytes / state.partBytes) ||
      part.bytes !==
        Math.min(state.partBytes, state.totalBytes - (part.partNumber - 1) * state.partBytes) ||
      part.etag.length < 1 ||
      part.etag.length > 1024 ||
      /[\u0000-\u001f\u007f]/.test(part.etag)
    )
      throw new RangeError("Invalid encrypted part metadata");
    const admission = this.ctx.storage.sql
      .exec<ChangeRow>(
        "DELETE FROM encrypted_upload_writes WHERE path = ? AND upload_id = ? AND part_number = ? RETURNING 1 AS changed",
        path,
        uploadId,
        part.partNumber,
      )
      .toArray();
    if (admission.length !== 1) return false;
    this.ctx.storage.sql.exec(
      "INSERT INTO encrypted_upload_parts(path, upload_id, part_number, etag, bytes) VALUES (?, ?, ?, ?, ?) ON CONFLICT(path, upload_id, part_number) DO UPDATE SET etag = excluded.etag, bytes = excluded.bytes",
      path,
      uploadId,
      part.partNumber,
      part.etag,
      part.bytes,
    );
    return true;
  }

  /** Unknown writes retain admission until explicit abort; a freeze cannot overtake R2 I/O. */
  async beginEncryptedPart(path: string, uploadId: string, partNumber: number): Promise<boolean> {
    const state = this.readEncryptedUpload(path, false);
    if (state?.uploadId !== uploadId || state.completing || state.failed) return false;
    if (
      !Number.isSafeInteger(partNumber) ||
      partNumber < 1 ||
      partNumber > Math.ceil(state.totalBytes / state.partBytes)
    )
      return false;
    return (
      this.ctx.storage.sql
        .exec<ChangeRow>(
          "INSERT OR IGNORE INTO encrypted_upload_writes(path, upload_id, part_number) VALUES (?, ?, ?) RETURNING 1 AS changed",
          path,
          uploadId,
          partNumber,
        )
        .toArray().length === 1
    );
  }

  async freezeEncryptedUpload(
    path: string,
    uploadId: string,
  ): Promise<{ state: EncryptedUploadState } | { error: "changed" | "incomplete" | "invalid" }> {
    const state = this.readEncryptedUpload(path);
    if (state === null || state.uploadId !== uploadId) return { error: "changed" };
    if (state.failed) return { error: "invalid" };
    const count = Math.ceil(state.totalBytes / state.partBytes);
    if (
      this.ctx.storage.sql
        .exec(
          "SELECT 1 FROM encrypted_upload_writes WHERE path = ? AND upload_id = ? LIMIT 1",
          path,
          uploadId,
        )
        .toArray().length !== 0 ||
      state.uploadedParts.length !== count ||
      state.uploadedParts.some(
        (part, index) =>
          part.partNumber !== index + 1 ||
          part.bytes !== Math.min(state.partBytes, state.totalBytes - index * state.partBytes),
      )
    )
      return { error: "incomplete" };
    state.completing = true;
    this.ctx.storage.sql.exec(
      "UPDATE encrypted_uploads SET value = ? WHERE path = ?",
      JSON.stringify({ ...state, uploadedParts: [] }),
      path,
    );
    return { state };
  }

  async rejectEncryptedUpload(path: string, uploadId: string): Promise<void> {
    const state = this.readEncryptedUpload(path, false);
    if (state?.uploadId !== uploadId || !state.completing) return;
    state.failed = true;
    this.ctx.storage.sql.exec(
      "UPDATE encrypted_uploads SET value = ? WHERE path = ?",
      JSON.stringify(state),
      path,
    );
  }

  /** Publication and cancellation compete in the same synchronous SQL transaction. */
  async publishEncryptedUpload(
    uploadId: string,
    record: EncryptedObjectRecord,
  ): Promise<"published" | "existing" | "changed"> {
    return this.ctx.storage.transactionSync(() => {
      const existing = this.ctx.storage.sql
        .exec<JsonRow>("SELECT value FROM encrypted_objects WHERE path = ?", record.path)
        .toArray()[0];
      const state = this.readEncryptedUpload(record.path, false);
      if (existing !== undefined) {
        if (state?.uploadId === uploadId) this.removeEncryptedUpload(record.path, uploadId);
        return "existing";
      }
      if (
        state?.uploadId !== uploadId ||
        state.key !== record.key ||
        state.totalBytes !== record.size ||
        !state.completing ||
        state.failed
      )
        return "changed";
      this.ctx.storage.sql.exec(
        "INSERT INTO encrypted_objects(path, value) VALUES (?, ?)",
        record.path,
        JSON.stringify(record),
      );
      this.removeEncryptedUpload(record.path, uploadId);
      return "published";
    });
  }

  async deleteEncryptedUpload(path: string, uploadId: string): Promise<void> {
    const state = this.readEncryptedUpload(path);
    if (state?.uploadId === uploadId) this.removeEncryptedUpload(path, uploadId);
  }

  async abortEncryptedUpload(path: string, uploadId: string): Promise<void> {
    const state = this.readEncryptedUpload(path, false);
    if (state?.uploadId === uploadId) this.removeEncryptedUpload(path, uploadId);
  }

  private removeEncryptedUpload(path: string, uploadId: string): void {
    this.ctx.storage.sql.exec("DELETE FROM encrypted_uploads WHERE path = ?", path);
    this.ctx.storage.sql.exec(
      "DELETE FROM encrypted_upload_parts WHERE path = ? AND upload_id = ?",
      path,
      uploadId,
    );
    this.ctx.storage.sql.exec(
      "DELETE FROM encrypted_upload_writes WHERE path = ? AND upload_id = ?",
      path,
      uploadId,
    );
  }

  private readEncryptedUpload(path: string, includeParts = true): EncryptedUploadState | null {
    const row = this.ctx.storage.sql
      .exec<JsonRow>("SELECT value FROM encrypted_uploads WHERE path = ?", path)
      .toArray()[0];
    if (row === undefined) return null;
    const state = JSON.parse(row.value) as EncryptedUploadState;
    if (includeParts)
      state.uploadedParts = this.ctx.storage.sql
        .exec<MultipartPartRow>(
          "SELECT p.part_number, p.etag, p.bytes FROM encrypted_upload_parts p WHERE p.path = ? AND p.upload_id = ? AND NOT EXISTS (SELECT 1 FROM encrypted_upload_writes w WHERE w.path = p.path AND w.upload_id = p.upload_id AND w.part_number = p.part_number) ORDER BY p.part_number",
          path,
          state.uploadId,
        )
        .toArray()
        .map((part) => ({ partNumber: part.part_number, etag: part.etag, bytes: part.bytes }));
    return state;
  }

  async headMetadata(path: string): Promise<number | null> {
    return this.readMetadata(path)?.byteLength ?? null;
  }

  async getMetadata(path: string): Promise<Uint8Array<ArrayBuffer> | null> {
    return this.readMetadata(path) ?? null;
  }

  async putMetadata(path: string, value: Uint8Array<ArrayBuffer>): Promise<void> {
    this.ctx.storage.sql.exec(
      `INSERT INTO metadata(path, value) VALUES (?, ?)
       ON CONFLICT(path) DO UPDATE SET value = excluded.value`,
      path,
      exactArrayBuffer(value),
    );
  }

  async deleteMetadata(path: string): Promise<void> {
    this.ctx.storage.sql.exec("DELETE FROM metadata WHERE path = ?", path);
  }

  async putMetadataIfAbsent(path: string, value: Uint8Array<ArrayBuffer>): Promise<boolean> {
    return (
      this.ctx.storage.sql
        .exec<ChangeRow>(
          "INSERT OR IGNORE INTO metadata(path, value) VALUES (?, ?) RETURNING 1 AS changed",
          path,
          exactArrayBuffer(value),
        )
        .toArray().length === 1
    );
  }

  async compareAndSwapMetadata(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
    replacement: Uint8Array<ArrayBuffer>,
  ): Promise<boolean> {
    const changed =
      expected === undefined
        ? this.ctx.storage.sql
            .exec<ChangeRow>(
              "INSERT OR IGNORE INTO metadata(path, value) VALUES (?, ?) RETURNING 1 AS changed",
              path,
              exactArrayBuffer(replacement),
            )
            .toArray().length
        : this.ctx.storage.sql
            .exec<ChangeRow>(
              "UPDATE metadata SET value = ? WHERE path = ? AND value = ? RETURNING 1 AS changed",
              exactArrayBuffer(replacement),
              path,
              exactArrayBuffer(expected),
            )
            .toArray().length;
    return changed === 1;
  }

  async compareAndDeleteMetadata(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
  ): Promise<boolean> {
    if (expected === undefined) {
      return this.readMetadata(path) === undefined;
    }
    return (
      this.ctx.storage.sql
        .exec<ChangeRow>(
          "DELETE FROM metadata WHERE path = ? AND value = ? RETURNING 1 AS changed",
          path,
          exactArrayBuffer(expected),
        )
        .toArray().length === 1
    );
  }

  async listMetadata(
    prefix: string,
    after: string | undefined,
    limit: number,
  ): Promise<MetadataListResult> {
    const rows = this.ctx.storage.sql
      .exec<PathRow>(
        `SELECT path FROM metadata
         WHERE substr(path, 1, ?) = ? AND path COLLATE BINARY > ? COLLATE BINARY
         ORDER BY path COLLATE BINARY
         LIMIT ?`,
        [...prefix].length,
        prefix,
        after ?? "",
        limit + 1,
      )
      .toArray()
      .map((row) => row.path);
    return { paths: rows.slice(0, limit), hasMore: rows.length > limit };
  }

  async getMultipartUpload(path: string): Promise<MultipartUploadState | null> {
    const upload = this.ctx.storage.sql
      .exec<MultipartUploadRow>(
        "SELECT upload_id, total_bytes, part_bytes FROM multipart_uploads WHERE path = ?",
        path,
      )
      .toArray()[0];
    if (upload === undefined) return null;
    const uploadedParts = this.ctx.storage.sql
      .exec<MultipartPartRow>(
        `SELECT part_number, etag, bytes FROM multipart_parts
         WHERE path = ? AND upload_id = ? ORDER BY part_number`,
        path,
        upload.upload_id,
      )
      .toArray()
      .map((part) => ({
        partNumber: part.part_number,
        etag: part.etag,
        bytes: part.bytes,
      }));
    return {
      uploadId: upload.upload_id,
      totalBytes: upload.total_bytes,
      partBytes: upload.part_bytes,
      uploadedParts,
    };
  }

  async createMultipartUpload(
    path: string,
    uploadId: string,
    totalBytes: number,
    partBytes: number,
  ): Promise<boolean> {
    validateMultipartIdentity(path, uploadId, totalBytes, partBytes);
    const now = Date.now();
    return (
      this.ctx.storage.sql
        .exec<ChangeRow>(
          `INSERT OR IGNORE INTO multipart_uploads(
             path, upload_id, total_bytes, part_bytes, created_at, updated_at
           ) VALUES (?, ?, ?, ?, ?, ?) RETURNING 1 AS changed`,
          path,
          uploadId,
          totalBytes,
          partBytes,
          now,
          now,
        )
        .toArray().length === 1
    );
  }

  async recordMultipartPart(
    path: string,
    uploadId: string,
    partNumber: number,
    etag: string,
    bytes: number,
  ): Promise<void> {
    const upload = await this.getMultipartUpload(path);
    if (upload === null || upload.uploadId !== uploadId) {
      throw new RangeError("Multipart upload session does not exist");
    }
    const partCount = Math.ceil(upload.totalBytes / upload.partBytes);
    const expectedBytes =
      partNumber === partCount
        ? upload.totalBytes - upload.partBytes * (partCount - 1)
        : upload.partBytes;
    if (
      !Number.isSafeInteger(partNumber) ||
      partNumber < 1 ||
      partNumber > partCount ||
      bytes !== expectedBytes ||
      etag.length < 1 ||
      etag.length > 1_024 ||
      /[\u0000-\u001f\u007f]/.test(etag)
    ) {
      throw new RangeError("Invalid multipart part metadata");
    }
    this.ctx.storage.sql.exec(
      `INSERT INTO multipart_parts(path, upload_id, part_number, etag, bytes)
       VALUES (?, ?, ?, ?, ?)
       ON CONFLICT(path, part_number) DO UPDATE SET
         upload_id = excluded.upload_id,
         etag = excluded.etag,
         bytes = excluded.bytes`,
      path,
      uploadId,
      partNumber,
      etag,
      bytes,
    );
    this.ctx.storage.sql.exec(
      "UPDATE multipart_uploads SET updated_at = ? WHERE path = ? AND upload_id = ?",
      Date.now(),
      path,
      uploadId,
    );
  }

  async deleteMultipartUpload(path: string, uploadId: string): Promise<void> {
    this.ctx.storage.sql.exec(
      "DELETE FROM multipart_parts WHERE path = ? AND upload_id = ?",
      path,
      uploadId,
    );
    this.ctx.storage.sql.exec(
      "DELETE FROM multipart_uploads WHERE path = ? AND upload_id = ?",
      path,
      uploadId,
    );
  }

  private readMetadata(path: string): Uint8Array<ArrayBuffer> | undefined {
    const row = this.ctx.storage.sql
      .exec<MetadataRow>("SELECT value FROM metadata WHERE path = ?", path)
      .toArray()[0];
    return row === undefined ? undefined : new Uint8Array(row.value.slice(0));
  }
}

function validateMultipartIdentity(
  path: string,
  uploadId: string,
  totalBytes: number,
  partBytes: number,
): void {
  if (
    path.length < 1 ||
    path.length > 2_048 ||
    uploadId.length < 1 ||
    uploadId.length > 1_024 ||
    /[\u0000-\u001f\u007f]/.test(path + uploadId) ||
    !Number.isSafeInteger(totalBytes) ||
    totalBytes < 1 ||
    !Number.isSafeInteger(partBytes) ||
    partBytes < 1
  ) {
    throw new RangeError("Invalid multipart upload metadata");
  }
}

function exactArrayBuffer(value: Uint8Array): ArrayBuffer {
  return new Uint8Array(value).buffer;
}
