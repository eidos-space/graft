import { DurableObject } from "cloudflare:workers";

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

  async putMetadataIfAbsent(
    path: string,
    value: Uint8Array<ArrayBuffer>,
  ): Promise<boolean> {
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
