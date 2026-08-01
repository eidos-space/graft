import {
  bytewiseCompare,
  isTransactionalPath,
  type GraftByteRange,
  type GraftListQuery,
  type GraftListResult,
  type GraftMultipartBackend,
  type GraftMultipartUpload,
  type GraftObject,
  type GraftObjectMetadata,
  type GraftRepositoryBackend,
  type GraftWriteBody,
  type GraftWriteOptions,
} from "@eidos.space/graft-remote";

import type { RepositoryDurableObject } from "./repository";

const R2_MIN_MULTIPART_PART_BYTES = 5 * 1024 * 1024;
const R2_MAX_MULTIPART_PART_BYTES = 5 * 1024 * 1024 * 1024;
const R2_MAX_MULTIPART_PARTS = 10_000;

export class CloudflareRepositoryBackend implements GraftRepositoryBackend {
  readonly #objects: R2Bucket;
  readonly #repositoryId: string;
  readonly #metadata: DurableObjectStub<RepositoryDurableObject>;
  readonly multipart: GraftMultipartBackend = {
    start: async (path, totalBytes, partBytes) =>
      await this.startMultipart(path, totalBytes, partBytes),
    uploadPart: async (path, uploadId, partNumber, value, contentLength) =>
      await this.uploadMultipartPart(path, uploadId, partNumber, value, contentLength),
    complete: async (path, uploadId) => await this.completeMultipart(path, uploadId),
    abort: async (path, uploadId) => await this.abortMultipart(path, uploadId),
  };

  constructor(storage: CloudflareRepositoryStorage, repositoryId: string) {
    this.#objects = storage.objects;
    this.#repositoryId = repositoryId;
    this.#metadata = storage.repositories.getByName(repositoryId);
  }

  async head(path: string): Promise<GraftObjectMetadata | null> {
    if (isTransactionalPath(path)) {
      const size = await this.#metadata.headMetadata(path);
      return size === null ? null : { size };
    }
    const object = await this.#objects.head(this.r2Key(path));
    return object === null ? null : { size: object.size, etag: object.httpEtag };
  }

  async get(path: string, range?: GraftByteRange): Promise<GraftObject | null> {
    if (isTransactionalPath(path)) {
      const value = await this.#metadata.getMetadata(path);
      if (value === null) {
        return null;
      }
      const body = range === undefined ? value : value.slice(range.start, range.end + 1);
      return { body, size: value.byteLength };
    }

    const object = await this.#objects.get(
      this.r2Key(path),
      range === undefined
        ? undefined
        : {
            range: { offset: range.start, length: range.end - range.start + 1 },
          },
    );
    if (object === null) {
      return null;
    }
    return {
      body: object.body,
      size: object.size,
      etag: object.httpEtag,
      ...(object.httpMetadata?.contentType === undefined
        ? {}
        : { contentType: object.httpMetadata.contentType }),
    };
  }

  async put(path: string, value: Uint8Array<ArrayBuffer>): Promise<void> {
    await this.#metadata.putMetadata(path, value);
  }

  async delete(path: string): Promise<void> {
    await this.#metadata.deleteMetadata(path);
  }

  async putIfAbsent(
    path: string,
    value: GraftWriteBody,
    kind: "transactional" | "immutable",
    options?: GraftWriteOptions,
  ): Promise<boolean> {
    if (kind === "transactional") {
      if (!(value instanceof Uint8Array)) {
        throw new TypeError("Transactional metadata must be buffered");
      }
      return await this.#metadata.putMetadataIfAbsent(path, new Uint8Array(value));
    }
    return await this.putImmutable(path, value, options);
  }

  async compareAndSwap(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
    replacement: Uint8Array<ArrayBuffer>,
  ): Promise<boolean> {
    return await this.#metadata.compareAndSwapMetadata(path, expected, replacement);
  }

  async compareAndDelete(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
  ): Promise<boolean> {
    return await this.#metadata.compareAndDeleteMetadata(path, expected);
  }

  async list(query: GraftListQuery): Promise<GraftListResult> {
    const candidateLimit = query.limit + 1;
    const after = query.after ?? "";
    const [metadata, immutablePage] = await Promise.all([
      this.#metadata.listMetadata(query.prefix, query.after, candidateLimit),
      this.#objects.list({
        prefix: this.r2Key(query.prefix),
        limit: candidateLimit,
        ...(query.after === undefined ? {} : { startAfter: this.r2Key(query.after) }),
      }),
    ]);
    const objectKeyPrefix = this.r2Key("");
    const immutable = immutablePage.objects
      .map((object) => object.key.slice(objectKeyPrefix.length))
      .filter((path) => path.startsWith(query.prefix) && bytewiseCompare(path, after) > 0);
    const candidates = [...new Set([...metadata.paths, ...immutable])].sort(bytewiseCompare);
    return {
      paths: candidates.slice(0, query.limit),
      hasMore: candidates.length > query.limit || metadata.hasMore || immutablePage.truncated,
    };
  }

  private r2Key(path: string): string {
    return `repositories/${this.#repositoryId}/objects/${path}`;
  }

  private async startMultipart(
    path: string,
    totalBytes: number,
    partBytes: number,
  ): Promise<GraftMultipartUpload | null> {
    validateR2MultipartShape(totalBytes, partBytes);
    const key = this.r2Key(path);
    if ((await this.#objects.head(key)) !== null) return null;

    const existing = await this.#metadata.getMultipartUpload(path);
    if (existing !== null) {
      if (existing.totalBytes === totalBytes && existing.partBytes === partBytes) {
        return publicMultipartUpload(existing);
      }
      await this.discardMultipart(path, existing.uploadId, key);
    }

    const upload = await this.#objects.createMultipartUpload(key, {
      httpMetadata: { contentType: "application/octet-stream" },
    });
    let retained = false;
    try {
      retained = await this.#metadata.createMultipartUpload(
        path,
        upload.uploadId,
        totalBytes,
        partBytes,
      );
      if (retained) {
        return {
          uploadId: upload.uploadId,
          totalBytes,
          partBytes,
          uploadedParts: [],
        };
      }
    } finally {
      if (!retained) {
        try {
          await upload.abort();
        } catch {
          // Another request won the durable session. R2 also expires abandoned uploads.
        }
      }
    }

    const winner = await this.#metadata.getMultipartUpload(path);
    if (
      winner === null ||
      winner.totalBytes !== totalBytes ||
      winner.partBytes !== partBytes
    ) {
      throw new Error("Multipart upload session changed while it was created");
    }
    return publicMultipartUpload(winner);
  }

  private async uploadMultipartPart(
    path: string,
    uploadId: string,
    partNumber: number,
    value: ReadableStream<Uint8Array>,
    contentLength: number,
  ): Promise<void> {
    const state = await this.requireMultipart(path, uploadId);
    const partCount = Math.ceil(state.totalBytes / state.partBytes);
    const expectedBytes =
      partNumber === partCount
        ? state.totalBytes - state.partBytes * (partCount - 1)
        : state.partBytes;
    if (partNumber < 1 || partNumber > partCount || contentLength !== expectedBytes) {
      throw new RangeError("Multipart part does not match the upload session");
    }

    const fixed = fixedR2Body(value, contentLength);
    try {
      const uploaded = await this.#objects
        .resumeMultipartUpload(this.r2Key(path), uploadId)
        .uploadPart(partNumber, fixed.body);
      await fixed.finish(true);
      await this.#metadata.recordMultipartPart(
        path,
        uploadId,
        uploaded.partNumber,
        uploaded.etag,
        contentLength,
      );
    } catch (error) {
      await fixed.cancel();
      await fixed.finish(false);
      throw error;
    }
  }

  private async completeMultipart(path: string, uploadId: string): Promise<boolean> {
    const key = this.r2Key(path);
    if ((await this.#objects.head(key)) !== null) {
      await this.discardMultipart(path, uploadId, key);
      return false;
    }
    const state = await this.requireMultipart(path, uploadId);
    const partCount = Math.ceil(state.totalBytes / state.partBytes);
    if (
      state.uploadedParts.length !== partCount ||
      state.uploadedParts.some((part, index) => part.partNumber !== index + 1)
    ) {
      throw new RangeError("Multipart upload is incomplete");
    }
    const object = await this.#objects
      .resumeMultipartUpload(key, uploadId)
      .complete(
        state.uploadedParts.map((part) => ({
          partNumber: part.partNumber,
          etag: part.etag,
        })),
      );
    if (object.size !== state.totalBytes) {
      throw new Error("Completed multipart object has an unexpected size");
    }
    await this.#metadata.deleteMultipartUpload(path, uploadId);
    return true;
  }

  private async abortMultipart(path: string, uploadId: string): Promise<void> {
    const state = await this.#metadata.getMultipartUpload(path);
    if (state === null || state.uploadId !== uploadId) return;
    await this.discardMultipart(path, uploadId, this.r2Key(path));
  }

  private async requireMultipart(path: string, uploadId: string) {
    const state = await this.#metadata.getMultipartUpload(path);
    if (state === null || state.uploadId !== uploadId) {
      throw new RangeError("Multipart upload session does not exist");
    }
    return state;
  }

  private async discardMultipart(path: string, uploadId: string, key: string): Promise<void> {
    try {
      await this.#objects.resumeMultipartUpload(key, uploadId).abort();
    } catch {
      // The upload may already be complete or expired. The durable session is stale either way.
    }
    await this.#metadata.deleteMultipartUpload(path, uploadId);
  }

  private async putImmutable(
    path: string,
    value: GraftWriteBody,
    options: GraftWriteOptions | undefined,
  ): Promise<boolean> {
    const fixed = fixedR2Body(value, options?.contentLength);
    try {
      const result = await this.#objects.put(this.r2Key(path), fixed.body, {
        onlyIf: new Headers({ "If-None-Match": "*" }),
        httpMetadata: { contentType: "application/octet-stream" },
      });
      if (result === null) await fixed.cancel();
      await fixed.finish(result !== null);
      return result !== null;
    } catch (error) {
      await fixed.cancel();
      await fixed.finish(false);
      throw error;
    }
  }
}

function publicMultipartUpload(
  state: Awaited<ReturnType<RepositoryDurableObject["getMultipartUpload"]>> & {},
): GraftMultipartUpload {
  return {
    uploadId: state.uploadId,
    totalBytes: state.totalBytes,
    partBytes: state.partBytes,
    uploadedParts: state.uploadedParts.map((part) => ({
      partNumber: part.partNumber,
      bytes: part.bytes,
    })),
  };
}

function validateR2MultipartShape(totalBytes: number, partBytes: number): void {
  if (
    !Number.isSafeInteger(totalBytes) ||
    totalBytes < 1 ||
    !Number.isSafeInteger(partBytes) ||
    partBytes < R2_MIN_MULTIPART_PART_BYTES ||
    partBytes > R2_MAX_MULTIPART_PART_BYTES ||
    Math.ceil(totalBytes / partBytes) > R2_MAX_MULTIPART_PARTS
  ) {
    throw new RangeError("Object is outside the supported R2 multipart limits");
  }
}

export interface CloudflareRepositoryStorage {
  objects: R2Bucket;
  repositories: DurableObjectNamespace<RepositoryDurableObject>;
}

interface FixedR2Body {
  body: GraftWriteBody;
  cancel(): Promise<void>;
  finish(consumed: boolean): Promise<void>;
}

function fixedR2Body(value: GraftWriteBody, contentLength: number | undefined): FixedR2Body {
  if (value instanceof Uint8Array || contentLength === undefined) {
    return {
      body: value,
      cancel: async () => undefined,
      finish: async () => undefined,
    };
  }
  const fixed = new FixedLengthStream(contentLength);
  const body = fixed.readable as ReadableStream<Uint8Array>;
  const completed = value.pipeTo(fixed.writable);
  void completed.catch(() => undefined);
  return {
    body,
    async cancel() {
      if (!body.locked) {
        try {
          await body.cancel("immutable target already exists or upload failed");
        } catch {
          // The R2 operation may already have canceled the fixed-length stream.
        }
      }
    },
    async finish(consumed) {
      try {
        await completed;
      } catch (error) {
        if (consumed) throw error;
      }
    },
  };
}
