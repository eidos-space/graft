import {
  GraftProtocolError,
  parseEncryptedRoutingHint,
  bytewiseCompare,
  type GraftByteRange,
  type GraftEncryptionDescriptor,
  type GraftListQuery,
  type GraftListResult,
  type GraftMultipartBackend,
  type GraftMultipartEncryptionIdentity,
  type GraftObject,
  type GraftObjectMetadata,
  type GraftRepositoryBackend,
  type GraftWriteBody,
  type GraftWriteOptions,
} from "@eidos.space/graft-remote";
import {
  CloudflareRepositoryBackend,
  fixedR2Body,
  type CloudflareRepositoryStorage,
} from "./backend.js";
import type { EncryptedUploadState, RepositoryDurableObject } from "./repository.js";

/** Open the durable profile; request input must never choose a repository's profile. */
export async function openCloudflareRepository(
  storage: CloudflareRepositoryStorage,
  id: string,
): Promise<GraftRepositoryBackend> {
  const metadata = storage.repositories.getByName(id);
  const profile = await metadata.getProfile();
  if (profile === null || profile.profile === "plain") {
    if (!(await metadata.registerProfile({ profile: "plain" }))) throw profileConflict();
    return new CloudflareRepositoryBackend(storage, id);
  }
  return new EncryptedCloudflareRepositoryBackend(storage, id, profile);
}

/** Call from an authorized, explicit repository-creation workflow. Never changes an existing profile. */
export async function createEncryptedCloudflareRepository(
  storage: CloudflareRepositoryStorage,
  id: string,
  uuid: string,
): Promise<GraftRepositoryBackend> {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid))
    throw new GraftProtocolError(400, "invalid_encryption_identity", "Invalid encryption UUID");
  const existing = await storage.objects.list({ prefix: `repositories/${id}/objects/`, limit: 1 });
  if (existing.objects.length !== 0)
    throw new GraftProtocolError(
      409,
      "repository_not_empty",
      "Encryption requires a new empty repository",
    );
  const encryption: GraftEncryptionDescriptor = {
    profile: "encrypted-remote-v1",
    version: 1,
    suite: 1,
    uuid,
  };
  if (!(await storage.repositories.getByName(id).registerProfile(encryption)))
    throw profileConflict();
  return new EncryptedCloudflareRepositoryBackend(storage, id, encryption);
}

/** Explicit maintenance operation; primitive reads remain available during rebuild. */
export async function rebuildCloudflareEncryptedRouting(
  storage: CloudflareRepositoryStorage,
  id: string,
): Promise<number> {
  const metadata = storage.repositories.getByName(id);
  const profile = await metadata.getProfile();
  if (profile === null || profile.profile === "plain") throw profileConflict();
  await metadata.resetEncryptedRouting();
  let after = "";
  let rebuilt = 0;
  for (;;) {
    const page = await metadata.listEncryptedObjects("e2ee/hints/", after, 64);
    if (page.length === 0) return rebuilt;
    for (const record of page) {
      if (record.path <= after || record.size > 1024 * 1024)
        throw new Error("Invalid retained routing hint");
      const object = await storage.objects.get(record.key);
      if (object === null || object.size !== record.size)
        throw new Error("Retained routing hint is unavailable");
      const bytes = new Uint8Array(record.size);
      const reader = object.body.getReader();
      let read = 0;
      try {
        for (;;) {
          const part = await reader.read();
          if (part.done) break;
          if (read + part.value.length > bytes.length)
            throw new Error("Invalid retained routing hint length");
          bytes.set(part.value, read);
          read += part.value.length;
        }
        if (read !== bytes.length) throw new Error("Incomplete retained routing hint");
      } finally {
        await reader.cancel().catch(() => undefined);
        reader.releaseLock();
      }
      const hint = parseEncryptedRoutingHint(record.path, bytes, profile);
      if (!(await metadata.restoreEncryptedRouting(record, hint)))
        throw new Error("Retained routing hint changed");
      after = record.path;
      rebuilt++;
    }
  }
}

class EncryptedCloudflareRepositoryBackend implements GraftRepositoryBackend {
  readonly #objects: R2Bucket;
  readonly #metadata: DurableObjectStub<RepositoryDurableObject>;
  readonly #prefix: string;
  readonly multipart: GraftMultipartBackend;
  readonly encryptedRouting: NonNullable<GraftRepositoryBackend["encryptedRouting"]>;

  constructor(
    storage: CloudflareRepositoryStorage,
    id: string,
    readonly encryption: GraftEncryptionDescriptor,
  ) {
    this.#objects = storage.objects;
    this.#metadata = storage.repositories.getByName(id);
    this.#prefix = `repositories/${id}/encrypted-staging/`;
    this.encryptedRouting = {
      select: (target, have) => this.#metadata.selectEncryptedHints(target, have),
    };
    this.multipart = {
      supportsEncryption: true,
      start: (path, total, part, identity) => this.start(path, total, part, identity),
      uploadPart: (path, upload, part, value, length) =>
        this.uploadPart(path, upload, part, value, length),
      complete: (path, upload) => this.complete(path, upload),
      abort: (path, upload) => this.abort(path, upload),
    };
  }

  async head(path: string): Promise<GraftObjectMetadata | null> {
    if (mutable(path)) {
      const size = await this.#metadata.headMetadata(path);
      return size === null ? null : { size };
    }
    const record = await this.#metadata.getEncryptedObject(path);
    return record === null ? null : { size: record.size };
  }

  async get(path: string, range?: GraftByteRange): Promise<GraftObject | null> {
    if (mutable(path)) {
      const value = await this.#metadata.getMetadata(path);
      return value === null
        ? null
        : {
            body: range === undefined ? value : value.slice(range.start, range.end + 1),
            size: value.length,
          };
    }
    const record = await this.#metadata.getEncryptedObject(path);
    if (record === null) return null;
    const object = await this.#objects.get(
      record.key,
      range === undefined
        ? undefined
        : { range: { offset: range.start, length: range.end - range.start + 1 } },
    );
    if (object === null || object.size !== record.size)
      throw new Error("Published encrypted object is unavailable");
    return { body: object.body, size: record.size };
  }

  async put(): Promise<void> {
    throw conditionalRequired();
  }
  async delete(): Promise<void> {
    throw conditionalRequired();
  }

  async putIfAbsent(
    path: string,
    value: GraftWriteBody,
    kind: "transactional" | "immutable",
    options?: GraftWriteOptions,
  ): Promise<boolean> {
    if (mutable(path)) {
      if (kind !== "transactional" || !(value instanceof Uint8Array)) throw conditionalRequired();
      return this.#metadata.putMetadataIfAbsent(path, value);
    }
    if (kind !== "immutable") throw conditionalRequired();
    if ((await this.#metadata.getEncryptedObject(path)) !== null) {
      if (value instanceof ReadableStream) await value.cancel();
      return false;
    }
    const key = this.#prefix + crypto.randomUUID();
    const fixed = fixedR2Body(value, options?.contentLength);
    try {
      const result = await this.#objects.put(key, fixed.body);
      await fixed.finish(result !== null);
      if (result === null) throw new Error("Encrypted staging upload failed");
      // A failed/unknown SQL response must retain this object: publication may have committed.
      const published = await this.#metadata.publishEncryptedObject(
        { path, key, size: result.size },
        options?.routingHint,
      );
      if (!published) {
        const winner = await this.#metadata.getEncryptedObject(path);
        if (winner?.key !== key) await this.#objects.delete(key);
      }
      return published;
    } catch (error) {
      await fixed.cancel();
      await fixed.finish(false);
      throw error;
    }
  }

  async compareAndSwap(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
    value: Uint8Array<ArrayBuffer>,
  ): Promise<boolean> {
    if (!mutable(path)) throw conditionalRequired();
    return this.#metadata.compareAndSwapMetadata(path, expected, value);
  }
  async compareAndDelete(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
  ): Promise<boolean> {
    if (!mutable(path)) throw conditionalRequired();
    return this.#metadata.compareAndDeleteMetadata(path, expected);
  }
  async list(query: GraftListQuery): Promise<GraftListResult> {
    const [metadata, objects] = await Promise.all([
      this.#metadata.listMetadata(query.prefix, query.after, query.limit + 1),
      this.#metadata.listEncryptedObjects(query.prefix, query.after ?? "", query.limit + 1),
    ]);
    const paths = [...metadata.paths, ...objects.map((object) => object.path)].sort(
      bytewiseCompare,
    );
    const selected = paths.slice(0, query.limit);
    return {
      paths: selected,
      hasMore: paths.length > query.limit || metadata.hasMore,
      entries: objects
        .filter((object) => selected.includes(object.path))
        .map(({ path, size }) => ({ path, size })),
    };
  }

  private async start(
    path: string,
    totalBytes: number,
    partBytes: number,
    encryption?: GraftMultipartEncryptionIdentity,
  ) {
    if (
      !/^e2ee\/(objects|packs|indexes|files|commits|segments)\/[0-9a-f]{64}$/.test(path) ||
      encryption?.uuid !== this.encryption.uuid ||
      !/^[0-9a-f]{64}$/.test(encryption.uploadId) ||
      !/^[0-9a-f]{64}$/.test(encryption.ciphertextSha256) ||
      !Number.isSafeInteger(totalBytes) ||
      totalBytes < 104 ||
      !Number.isSafeInteger(partBytes) ||
      partBytes < 5 * 1024 * 1024 ||
      partBytes > 5 * 1024 * 1024 * 1024 ||
      Math.ceil(totalBytes / partBytes) > 10_000
    )
      throw new GraftProtocolError(
        400,
        "invalid_encrypted_upload",
        "Invalid encrypted multipart identity or size",
      );
    if ((await this.#metadata.getEncryptedObject(path)) !== null) return null;
    let state = await this.#metadata.getEncryptedUpload(path);
    if (state === null) {
      const key = this.#prefix + crypto.randomUUID();
      const upload = await this.#objects.createMultipartUpload(key);
      const candidate: EncryptedUploadState = {
        path,
        key,
        uploadId: upload.uploadId,
        totalBytes,
        partBytes,
        encryption,
        completing: false,
        uploadedParts: [],
      };
      // Retain an unknown outcome; never abort an upload whose allocation may have committed.
      if (await this.#metadata.createEncryptedUpload(candidate)) state = candidate;
      else {
        await upload.abort();
        state = await this.#metadata.getEncryptedUpload(path);
      }
    }
    if (
      state === null ||
      state.totalBytes !== totalBytes ||
      state.partBytes !== partBytes ||
      !sameIdentity(state.encryption, encryption)
    )
      throw new GraftProtocolError(
        409,
        "encrypted_upload_conflict",
        "Another ciphertext upload owns this key",
      );
    return {
      uploadId: state.uploadId,
      totalBytes,
      partBytes,
      encryption: state.encryption,
      uploadedParts: state.uploadedParts.map(({ partNumber, bytes }) => ({ partNumber, bytes })),
    };
  }

  private async requireUpload(path: string, uploadId: string): Promise<EncryptedUploadState> {
    const state = await this.#metadata.getEncryptedUpload(path);
    if (
      state === null ||
      state.uploadId !== uploadId ||
      state.encryption.uuid !== this.encryption.uuid
    )
      throw new GraftProtocolError(
        409,
        "upload_session_changed",
        "Encrypted upload session is absent",
      );
    return state;
  }
  private async uploadPart(
    path: string,
    uploadId: string,
    partNumber: number,
    value: ReadableStream<Uint8Array>,
    length: number,
  ): Promise<void> {
    const state = await this.requireUpload(path, uploadId);
    if (
      state.completing ||
      !Number.isSafeInteger(partNumber) ||
      partNumber < 1 ||
      partNumber > Math.ceil(state.totalBytes / state.partBytes) ||
      length !== Math.min(state.partBytes, state.totalBytes - (partNumber - 1) * state.partBytes)
    )
      throw new GraftProtocolError(
        400,
        "invalid_multipart_part",
        "Invalid encrypted multipart part",
      );
    const fixed = fixedR2Body(value, length);
    try {
      if (!(await this.#metadata.beginEncryptedPart(path, uploadId, partNumber)))
        throw new GraftProtocolError(
          409,
          "upload_part_pending",
          "Encrypted part is pending, or its session changed; reconcile or explicitly abort",
        );
      const part = await this.#objects
        .resumeMultipartUpload(state.key, uploadId)
        .uploadPart(partNumber, fixed.body);
      await fixed.finish(true);
      if (
        !(await this.#metadata.recordEncryptedPart(path, uploadId, {
          partNumber,
          etag: part.etag,
          bytes: length,
        }))
      )
        throw new GraftProtocolError(
          409,
          "upload_session_changed",
          "Multipart session is absent or completing",
        );
    } catch (error) {
      await fixed.cancel();
      await fixed.finish(false);
      throw error;
    }
  }
  private async complete(path: string, uploadId: string): Promise<boolean> {
    const existing = await this.#metadata.getEncryptedObject(path);
    if (existing !== null) return false;
    const frozen = await this.#metadata.freezeEncryptedUpload(path, uploadId);
    if ("error" in frozen)
      throw new GraftProtocolError(
        frozen.error === "changed" ? 409 : 400,
        "invalid_upload_state",
        "Encrypted multipart session is absent, incomplete or invalid",
      );
    const state = frozen.state;
    if ((await this.#objects.head(state.key)) === null) {
      try {
        await this.#objects
          .resumeMultipartUpload(state.key, uploadId)
          .complete(state.uploadedParts);
      } catch (error) {
        if ((await this.#objects.head(state.key)) === null) throw error;
      }
    }
    const staged = await this.#objects.get(state.key);
    if (staged === null) throw new Error("Encrypted staging object is unavailable");
    try {
      if (staged.size !== state.totalBytes) {
        await staged.body.cancel();
        throw invalidCiphertext();
      }
      await verifyCiphertext(staged.body, state);
    } catch (error) {
      if (error instanceof GraftProtocolError && error.code === "invalid_encrypted_ciphertext")
        await this.#metadata.rejectEncryptedUpload(path, uploadId);
      throw error;
    }
    const publication = await this.#metadata.publishEncryptedUpload(uploadId, {
      path,
      key: state.key,
      size: state.totalBytes,
    });
    if (publication === "changed")
      throw new GraftProtocolError(
        409,
        "upload_session_changed",
        "Encrypted upload was rejected or aborted before publication",
      );
    // Preserve staging on an unknown publication response. A confirmed loser is safe to discard.
    if (publication === "existing") {
      const winner = await this.#metadata.getEncryptedObject(path);
      if (winner?.key !== state.key) await this.#objects.delete(state.key);
    }
    return publication === "published";
  }
  private async abort(path: string, uploadId: string): Promise<void> {
    const state = await this.#metadata.getEncryptedUpload(path);
    if (state === null || state.uploadId !== uploadId) return;
    // Removal fences both future freezes and already-running completion verifiers.
    // Unknown metadata outcomes retain all R2 data for reconciliation.
    await this.#metadata.abortEncryptedUpload(path, uploadId);
    const winner = await this.#metadata.getEncryptedObject(path);
    if (winner?.key === state.key) return;
    try {
      await this.#objects.resumeMultipartUpload(state.key, uploadId).abort();
    } catch {
      /* Expired incomplete uploads have no public object. */
    }
    await this.#objects.delete(state.key);
  }
}

function mutable(path: string): boolean {
  return path === "e2ee/config" || /^e2ee\/(refs|head)\/[0-9a-f]{64}$/.test(path);
}
function conditionalRequired() {
  return new GraftProtocolError(
    400,
    "encrypted_mutation_requires_condition",
    "Encrypted object requires the appropriate conditional operation",
  );
}
function profileConflict() {
  return new GraftProtocolError(
    409,
    "profile_mismatch",
    "Repository profile is already registered or not empty",
  );
}
function invalidCiphertext() {
  return new GraftProtocolError(
    400,
    "invalid_encrypted_ciphertext",
    "Assembled ciphertext does not match the retained upload identity",
  );
}
function sameIdentity(
  a: GraftMultipartEncryptionIdentity,
  b: GraftMultipartEncryptionIdentity,
): boolean {
  return (
    a.uuid === b.uuid && a.uploadId === b.uploadId && a.ciphertextSha256 === b.ciphertextSha256
  );
}
function hex(bytes: Uint8Array): string {
  return [...bytes].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
async function verifyCiphertext(
  body: ReadableStream<Uint8Array>,
  state: EncryptedUploadState,
): Promise<void> {
  // lib.webworker's Crypto type omits the Workers streaming digest extension.
  const workerCrypto = crypto as Crypto & { DigestStream: typeof DigestStream };
  const digest = new workerCrypto.DigestStream("SHA-256");
  const writer = digest.getWriter();
  const reader = body.getReader();
  const prefix = new Uint8Array(72);
  let received = 0;
  try {
    while (true) {
      const part = await reader.read();
      if (part.done) break;
      if (received < prefix.length)
        prefix.set(part.value.subarray(0, prefix.length - received), received);
      received += part.value.length;
      if (received > state.totalBytes) throw invalidCiphertext();
      await writer.write(part.value);
    }
    await writer.close();
    if (
      received !== state.totalBytes ||
      hex(new Uint8Array(await digest.digest)) !== state.encryption.ciphertextSha256 ||
      new TextDecoder().decode(prefix.subarray(0, 8)) !== "GRAFTEO1" ||
      prefix[8] !== 0 ||
      prefix[9] !== 1 ||
      prefix[10] !== 0 ||
      prefix[11] !== 1 ||
      hex(prefix.subarray(12, 44)) !== state.encryption.uploadId
    )
      throw invalidCiphertext();
    const header = new DataView(prefix.buffer).getUint32(68);
    if (header < 16 || header > 16384 || received < 72 + header + 16) throw invalidCiphertext();
  } catch (error) {
    await writer.abort(error).catch(() => undefined);
    await digest.digest.catch(() => undefined);
    throw error;
  } finally {
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
    writer.releaseLock();
  }
}
