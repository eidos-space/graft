import { describe, expect, it } from "vitest";

import {
  GraftProtocolError,
  bytewiseCompare,
  bytesEqual,
  createGraftRemoteHandler,
  type GraftByteRange,
  type GraftListQuery,
  type GraftMultipartBackend,
  type GraftObject,
  type GraftObjectMetadata,
  type GraftRepositoryBackend,
  type GraftWriteBody,
} from "../src/index.js";

const ORIGIN = "https://remote.example";

class MemoryRepository implements GraftRepositoryBackend {
  readonly objects = new Map<string, Uint8Array<ArrayBuffer>>();
  readonly uploads = new Map<
    string,
    {
      uploadId: string;
      totalBytes: number;
      partBytes: number;
      parts: Map<number, Uint8Array<ArrayBuffer>>;
    }
  >();
  readonly multipart: GraftMultipartBackend = {
    start: (path, totalBytes, partBytes) => {
      if (this.objects.has(path)) return null;
      const existing = this.uploads.get(path);
      if (existing !== undefined) {
        return {
          uploadId: existing.uploadId,
          totalBytes: existing.totalBytes,
          partBytes: existing.partBytes,
          uploadedParts: [...existing.parts]
            .sort(([left], [right]) => left - right)
            .map(([partNumber, bytes]) => ({ partNumber, bytes: bytes.byteLength })),
        };
      }
      const upload = {
        uploadId: crypto.randomUUID(),
        totalBytes,
        partBytes,
        parts: new Map<number, Uint8Array<ArrayBuffer>>(),
      };
      this.uploads.set(path, upload);
      return { uploadId: upload.uploadId, totalBytes, partBytes, uploadedParts: [] };
    },
    uploadPart: async (path, uploadId, partNumber, value) => {
      const upload = this.uploads.get(path);
      if (upload === undefined || upload.uploadId !== uploadId) {
        throw new RangeError("unknown multipart upload");
      }
      upload.parts.set(partNumber, await bodyBytes(value));
    },
    complete: (path, uploadId) => {
      if (this.objects.has(path)) return false;
      const upload = this.uploads.get(path);
      if (upload === undefined || upload.uploadId !== uploadId) {
        throw new RangeError("unknown multipart upload");
      }
      const value = new Uint8Array(upload.totalBytes);
      let offset = 0;
      for (const [, part] of [...upload.parts].sort(([left], [right]) => left - right)) {
        value.set(part, offset);
        offset += part.byteLength;
      }
      if (offset !== value.byteLength) throw new RangeError("incomplete multipart upload");
      this.objects.set(path, value);
      this.uploads.delete(path);
      return true;
    },
    abort: (path, uploadId) => {
      if (this.uploads.get(path)?.uploadId === uploadId) this.uploads.delete(path);
    },
  };

  head(path: string): GraftObjectMetadata | null {
    const value = this.objects.get(path);
    return value === undefined ? null : { size: value.byteLength };
  }

  get(
    path: string,
    range?: GraftByteRange,
  ): GraftObject | null | Promise<GraftObject | null> {
    const value = this.objects.get(path);
    if (value === undefined) {
      return null;
    }
    const body = range === undefined ? value.slice() : value.slice(range.start, range.end + 1);
    return { body, size: value.byteLength };
  }

  put(path: string, value: Uint8Array<ArrayBuffer>): void {
    this.objects.set(path, value.slice());
  }

  delete(path: string): void {
    this.objects.delete(path);
  }

  async putIfAbsent(path: string, value: GraftWriteBody): Promise<boolean> {
    if (this.objects.has(path)) {
      return false;
    }
    this.objects.set(path, await bodyBytes(value));
    return true;
  }

  compareAndSwap(
    path: string,
    expected: Uint8Array<ArrayBuffer> | undefined,
    replacement: Uint8Array<ArrayBuffer>,
  ): boolean {
    const current = this.objects.get(path);
    if (!expectedMatches(current, expected)) {
      return false;
    }
    this.objects.set(path, replacement.slice());
    return true;
  }

  compareAndDelete(path: string, expected: Uint8Array<ArrayBuffer> | undefined): boolean {
    const current = this.objects.get(path);
    if (!expectedMatches(current, expected)) {
      return false;
    }
    if (expected !== undefined) {
      this.objects.delete(path);
    }
    return true;
  }

  list(query: GraftListQuery): { paths: string[]; hasMore: boolean } {
    const matching = [...this.objects.keys()]
      .filter(
        (path) =>
          path.startsWith(query.prefix) &&
          (query.after === undefined || bytewiseCompare(path, query.after) > 0),
      )
      .sort(bytewiseCompare);
    return {
      paths: matching.slice(0, query.limit),
      hasMore: matching.length > query.limit,
    };
  }
}

function createTestApp(limits?: { maxRequestBytes?: number; multipartPartBytes?: number }) {
  const repositories = new Map<string, MemoryRepository>();
  return createGraftRemoteHandler({
    authenticate({ request }) {
      if (request.headers.get("Authorization") !== "Bearer test-token") {
        throw new GraftProtocolError(401, "unauthorized", "A valid bearer token is required", {
          "WWW-Authenticate": 'Bearer realm="graft-remote"',
        });
      }
    },
    backend({ repository }) {
      let backend = repositories.get(repository.id);
      if (backend === undefined) {
        backend = new MemoryRepository();
        repositories.set(repository.id, backend);
      }
      return backend;
    },
    limits,
  });
}

async function remoteFetch(
  app: ReturnType<typeof createTestApp>,
  path: string,
  init: RequestInit = {},
  options: { token?: string; protocol?: string } = {},
): Promise<Response> {
  const headers = new Headers(init.headers);
  headers.set("Authorization", `Bearer ${options.token ?? "test-token"}`);
  headers.set("Graft-Protocol", options.protocol ?? "1");
  return await handlerFetch(app, path, { ...init, headers });
}

describe("createGraftRemoteHandler", () => {
  it("negotiates authentication and protocol without a framework dependency", async () => {
    const app = createTestApp();

    const unauthorized = await handlerFetch(app, "/acme/archive", {
      headers: { "Graft-Protocol": "1" },
    });
    expect(unauthorized.status).toBe(401);
    expect(unauthorized.headers.get("www-authenticate")).toContain("Bearer");
    expect(unauthorized.headers.get("graft-protocol")).toBe("1");

    const unsupported = await remoteFetch(app, "/acme/archive", {}, { protocol: "2" });
    expect(unsupported.status).toBe(426);

    const descriptor = await remoteFetch(app, "/acme/archive");
    expect(descriptor.status).toBe(200);
    expect(await descriptor.json()).toMatchObject({
      protocol: "graft-remote",
      version: 1,
      repository: "acme/archive",
      capabilities: expect.arrayContaining([
        "range",
        "list",
        "upload-bundle",
        "receive-pack",
        "receive-bundle",
        "cas",
      ]),
    });
  });

  it("resumes multipart immutable uploads and advertises request limits", async () => {
    const app = createTestApp({ maxRequestBytes: 8, multipartPartBytes: 5 });
    const descriptor = await remoteFetch(app, "/acme/archive");
    expect(await descriptor.json()).toMatchObject({
      capabilities: expect.arrayContaining(["multipart-object"]),
      limits: { max_request_bytes: 8, multipart_part_bytes: 5 },
    });

    const start = await remoteFetch(app, "/acme/archive/multipart-start/segments/large", {
      method: "POST",
      headers: { "x-graft-object-bytes": "11", "Content-Length": "0" },
    });
    expect(start.status).toBe(200);
    const session = (await start.json()) as { upload_id: string };

    expect(
      (
        await remoteFetch(app, "/acme/archive/multipart-part/segments/large", {
          method: "PUT",
          headers: {
            "x-graft-upload-id": session.upload_id,
            "x-graft-part-number": "1",
            "Content-Length": "5",
          },
          body: "abcde",
        })
      ).status,
    ).toBe(204);

    const resumed = await remoteFetch(app, "/acme/archive/multipart-start/segments/large", {
      method: "POST",
      headers: { "x-graft-object-bytes": "11", "Content-Length": "0" },
    });
    expect(await resumed.json()).toMatchObject({
      upload_id: session.upload_id,
      uploaded_parts: [{ part_number: 1, bytes: 5 }],
    });

    for (const [partNumber, body] of [
      [2, "fghij"],
      [3, "k"],
    ] as const) {
      const part = await remoteFetch(app, "/acme/archive/multipart-part/segments/large", {
        method: "PUT",
        headers: {
          "x-graft-upload-id": session.upload_id,
          "x-graft-part-number": partNumber.toString(),
          "Content-Length": body.length.toString(),
        },
        body,
      });
      expect(part.status).toBe(204);
    }

    const complete = await remoteFetch(app, "/acme/archive/multipart-complete/segments/large", {
      method: "POST",
      headers: { "x-graft-upload-id": session.upload_id, "Content-Length": "0" },
    });
    expect(complete.status).toBe(204);
    expect(
      await (await remoteFetch(app, "/acme/archive/raw/segments/large")).text(),
    ).toBe("abcdefghijk");
    expect(
      (
        await remoteFetch(app, "/acme/archive/multipart-start/segments/large", {
          method: "POST",
          headers: { "x-graft-object-bytes": "11", "Content-Length": "0" },
        })
      ).status,
    ).toBe(412);

    const tooLarge = await remoteFetch(app, "/acme/archive/raw-if-not-exists/segments/direct", {
      method: "PUT",
      headers: { "Content-Length": "9" },
      body: "123456789",
    });
    expect(tooLarge.status).toBe(413);
    expect(await tooLarge.json()).toMatchObject({ title: "request_too_large" });
  });

  it("separates authentication, authorization, repository mapping, and storage", async () => {
    const backend = new MemoryRepository();
    const authorized: Array<{
      action: string;
      principal: string;
      repository: string;
    }> = [];
    let backendOpens = 0;
    const app = createGraftRemoteHandler<undefined, string>({
      authenticate: () => "user-1",
      repositoryId: ({ namespace, name }) => `tenant-7:${namespace}/${name}`,
      authorize({ action, principal, repository }) {
        if (principal === undefined) {
          throw new Error("authenticated principal is missing");
        }
        authorized.push({ action, principal, repository: repository.id });
        if (action === "write") {
          throw new GraftProtocolError(403, "forbidden", "Write access denied");
        }
      },
      backend() {
        backendOpens += 1;
        return backend;
      },
    });
    const headers = { Authorization: "Bearer ignored", "Graft-Protocol": "1" };

    const descriptor = await handlerFetch(app, "/acme/archive", { headers });
    expect(descriptor.status).toBe(200);
    expect(await descriptor.json()).toMatchObject({
      repository: "tenant-7:acme/archive",
    });

    const denied = await handlerFetch(app, "/acme/archive/raw/HEAD", {
      method: "PUT",
      headers,
      body: "main\n",
    });
    expect(denied.status).toBe(403);
    expect(backendOpens).toBe(1);
    expect(authorized).toEqual([
      {
        action: "discover",
        principal: "user-1",
        repository: "tenant-7:acme/archive",
      },
      {
        action: "write",
        principal: "user-1",
        repository: "tenant-7:acme/archive",
      },
    ]);
  });

  it("streams immutable objects with create-only and range semantics", async () => {
    const app = createTestApp();
    const path = "/objects/repo/raw-if-not-exists/objects/pack/data.pack";
    expect((await remoteFetch(app, path, { method: "PUT", body: "abcdef" })).status).toBe(204);
    expect((await remoteFetch(app, path, { method: "PUT", body: "changed" })).status).toBe(412);

    const full = await remoteFetch(app, "/objects/repo/raw/objects/pack/data.pack");
    expect(await full.text()).toBe("abcdef");

    const range = await remoteFetch(app, "/objects/repo/raw/objects/pack/data.pack", {
      headers: { Range: "bytes=1-3" },
    });
    expect(range.status).toBe(206);
    expect(range.headers.get("content-range")).toBe("bytes 1-3/6");
    expect(await range.text()).toBe("bcd");

    const unsatisfiable = await remoteFetch(app, "/objects/repo/raw/objects/pack/data.pack", {
      headers: { Range: "bytes=99-100" },
    });
    expect(unsatisfiable.status).toBe(416);
    expect(unsatisfiable.headers.get("content-range")).toBe("bytes */6");
  });

  it("performs atomic CAS and CAD for transactional metadata", async () => {
    const app = createTestApp();
    const ref = "/cas/repo/cas/refs/heads/main";
    const created = await remoteFetch(app, ref, {
      method: "POST",
      headers: {
        "x-graft-expected-present": "false",
        "x-graft-expected-hex": "",
      },
      body: "a\n",
    });
    expect(created.status).toBe(204);

    const contenders = await Promise.all(
      ["b\n", "c\n"].map((body) =>
        remoteFetch(app, ref, {
          method: "POST",
          headers: {
            "x-graft-expected-present": "true",
            "x-graft-expected-hex": "610a",
          },
          body,
        }),
      ),
    );
    expect(contenders.map((response) => response.status).sort()).toEqual([204, 409]);

    const current = await remoteFetch(app, "/cas/repo/raw/refs/heads/main");
    const value = await current.text();
    const expectedHex = value === "b\n" ? "620a" : "630a";
    expect(
      (
        await remoteFetch(app, "/cas/repo/cad/refs/heads/main", {
          method: "POST",
          headers: {
            "x-graft-expected-present": "true",
            "x-graft-expected-hex": expectedHex,
          },
        })
      ).status,
    ).toBe(204);
  });

  it("publishes a pack, index, and ref with one receive-pack request", async () => {
    const app = createTestApp();
    const packId = "a".repeat(64);
    const path = "/receive/repo/receive-pack/refs/heads/main";
    const first = await remoteFetch(app, path, {
      method: "POST",
      headers: receivePackHeaders(packId, undefined, "new\n", 4, 3),
      body: "packidx",
    });
    expect(first.status, await first.clone().text()).toBe(204);

    expect(
      await (await remoteFetch(app, `/receive/repo/raw/objects/pack/${packId}.pack`)).text(),
    ).toBe("pack");
    expect(
      await (await remoteFetch(app, `/receive/repo/raw/objects/pack/${packId}.idx`)).text(),
    ).toBe("idx");
    expect(await (await remoteFetch(app, "/receive/repo/raw/refs/heads/main")).text()).toBe(
      "new\n",
    );

    const retry = await remoteFetch(app, path, {
      method: "POST",
      headers: receivePackHeaders(packId, "new\n", "next\n", 4, 3),
      body: "ignored",
    });
    expect(retry.status, await retry.clone().text()).toBe(204);
    expect(await (await remoteFetch(app, "/receive/repo/raw/refs/heads/main")).text()).toBe(
      "next\n",
    );
    expect(
      await (await remoteFetch(app, `/receive/repo/raw/objects/pack/${packId}.pack`)).text(),
    ).toBe("pack");
  });

  it("streams a ref snapshot and immutable objects in one upload-bundle request", async () => {
    const app = createTestApp();
    for (const [path, body] of [
      ["refs/heads/main", "commit-1\n"],
      ["objects/pack/one.idx", "index"],
      ["objects/pack/one.pack", "pack-data"],
      ["segments/one", "segment"],
    ] as const) {
      const operation = path.startsWith("refs/") ? "raw" : "raw-if-not-exists";
      expect(
        (
          await remoteFetch(app, `/upload/repo/${operation}/${path}`, {
            method: "PUT",
            body,
          })
        ).status,
      ).toBe(204);
    }

    const response = await remoteFetch(app, "/upload/repo/upload-bundle/refs/heads/main", {
      method: "POST",
    });
    expect(response.status, await response.clone().text()).toBe(200);
    expect(response.headers.get("content-type")).toBe("application/vnd.graft.upload-bundle");
    const bytes = new Uint8Array(await response.arrayBuffer());
    const manifestBytes = Number(response.headers.get("x-graft-bundle-manifest-bytes"));
    const manifest = JSON.parse(new TextDecoder().decode(bytes.subarray(0, manifestBytes))) as {
      version: number;
      reference: { path: string; value_hex: string };
      objects: number;
    };
    expect(manifest).toEqual({
      version: 1,
      reference: { path: "refs/heads/main", value_hex: "636f6d6d69742d310a" },
      objects: 3,
    });
    expect(decodeUploadBundleFrames(bytes.subarray(manifestBytes), manifest.objects)).toEqual([
      ["objects/pack/one.idx", "index"],
      ["objects/pack/one.pack", "pack-data"],
      ["segments/one", "segment"],
    ]);
  });

  it("prefetches upload-bundle objects with a bounded concurrency window", async () => {
    class DelayedRepository extends MemoryRepository {
      activeImmutableGets = 0;
      maximumImmutableGets = 0;

      override async get(path: string, range?: GraftByteRange): Promise<GraftObject | null> {
        if (!path.startsWith("refs/")) {
          this.activeImmutableGets += 1;
          this.maximumImmutableGets = Math.max(
            this.maximumImmutableGets,
            this.activeImmutableGets,
          );
          await new Promise((resolve) => setTimeout(resolve, 5));
          this.activeImmutableGets -= 1;
        }
        return await super.get(path, range);
      }
    }

    const backend = new DelayedRepository();
    backend.put("refs/heads/main", new TextEncoder().encode("commit-1\n"));
    for (let index = 0; index < 12; index += 1) {
      backend.put(
        `objects/prefetch/${index.toString().padStart(2, "0")}`,
        new Uint8Array([index]),
      );
    }
    const app = createGraftRemoteHandler({ backend: () => backend });
    const response = await remoteFetch(app, "/prefetch/repo/upload-bundle/refs/heads/main", {
      method: "POST",
    });
    expect(response.status, await response.clone().text()).toBe(200);
    await response.arrayBuffer();
    expect(backend.maximumImmutableGets).toBe(8);
  });

  it("returns not found when upload-bundle cannot resolve the requested ref", async () => {
    const response = await remoteFetch(
      createTestApp(),
      "/missing/repo/upload-bundle/refs/heads/main",
      { method: "POST" },
    );
    expect(response.status).toBe(404);
  });

  it("does not publish a ref when a receive-pack body is truncated", async () => {
    const app = createTestApp();
    const packId = "b".repeat(64);
    const response = await remoteFetch(app, "/partial/repo/receive-pack/refs/heads/main", {
      method: "POST",
      headers: receivePackHeaders(packId, undefined, "new\n", 4, 3, 6),
      body: "packid",
    });
    expect(response.status).toBe(400);
    expect((await remoteFetch(app, "/partial/repo/raw/refs/heads/main")).status).toBe(404);
  });

  it("publishes bundled immutable objects, pack, index, and ref in one request", async () => {
    const app = createTestApp();
    const packId = "c".repeat(64);
    const manifest = new TextEncoder().encode(
      JSON.stringify({
        version: 1,
        objects: [
          { path: "segments/example", bytes: 7, allow_existing: true },
          {
            path: "logs/example/commits/0000000000000001",
            bytes: 6,
            allow_existing: false,
          },
        ],
      }),
    );
    const body = joinBytes([manifest, new TextEncoder().encode("segmentcommitpackidx")]);
    const path = "/bundle/repo/receive-bundle/refs/heads/main";
    const first = await remoteFetch(app, path, {
      method: "POST",
      headers: receiveBundleHeaders(packId, undefined, "new\n", manifest, 4, 3, body.byteLength),
      body,
    });
    expect(first.status, await first.clone().text()).toBe(204);
    expect(await (await remoteFetch(app, "/bundle/repo/raw/segments/example")).text()).toBe(
      "segment",
    );
    expect(
      await (
        await remoteFetch(app, "/bundle/repo/raw/logs/example/commits/0000000000000001")
      ).text(),
    ).toBe("commit");
    expect(await (await remoteFetch(app, "/bundle/repo/raw/refs/heads/main")).text()).toBe("new\n");

    const retry = await remoteFetch(app, path, {
      method: "POST",
      headers: receiveBundleHeaders(packId, "new\n", "next\n", manifest, 4, 3, body.byteLength),
      body,
    });
    expect(retry.status).toBe(412);
    expect(await (await remoteFetch(app, "/bundle/repo/raw/refs/heads/main")).text()).toBe("new\n");
  });

  it("owns cursor pagination while the backend only lists ordered paths", async () => {
    const app = createTestApp();
    for (const path of ["objects/aa/one", "objects/bb/two", "objects/cc/three"]) {
      expect(
        (
          await remoteFetch(app, `/list/repo/raw-if-not-exists/${path}`, {
            method: "PUT",
            body: path,
          })
        ).status,
      ).toBe(204);
    }

    const paths: string[] = [];
    let cursor: string | undefined;
    do {
      const query = new URLSearchParams({ limit: "2" });
      if (cursor === undefined) query.set("prefix", "objects/");
      else query.set("cursor", cursor);
      const response = await remoteFetch(app, `/list/repo/list?${query}`);
      const page = (await response.json()) as {
        paths: string[];
        next_cursor?: string;
      };
      paths.push(...page.paths);
      cursor = page.next_cursor;
    } while (cursor !== undefined);
    expect(paths).toEqual(["objects/aa/one", "objects/bb/two", "objects/cc/three"]);
  });

  it("isolates repositories and rejects unsafe paths", async () => {
    const app = createTestApp();
    await remoteFetch(app, "/isolation/one/raw-if-not-exists/HEAD", {
      method: "PUT",
      body: "one",
    });
    await remoteFetch(app, "/isolation/two/raw-if-not-exists/HEAD", {
      method: "PUT",
      body: "two",
    });
    expect(await (await remoteFetch(app, "/isolation/one/raw/HEAD")).text()).toBe("one");
    expect(await (await remoteFetch(app, "/isolation/two/raw/HEAD")).text()).toBe("two");

    const reserved = await remoteFetch(app, "/safety/repo/raw-if-not-exists/locks/ref.lock", {
      method: "PUT",
      body: "value",
    });
    expect(reserved.status).toBe(400);

    const encodedSlash = await remoteFetch(app, "/safety/repo/raw-if-not-exists/objects%2Fhidden", {
      method: "PUT",
      body: "value",
    });
    expect(encodedSlash.status).toBe(400);
  });

  it("does not log backend or error-reporter details", async () => {
    const messages: string[] = [];
    const originalError = console.error;
    console.error = (...values: unknown[]) => messages.push(values.join(" "));
    try {
      const app = createGraftRemoteHandler({
        backend() {
          throw new Error("storage-secret repository/path");
        },
        onError() {
          throw new Error("reporter-secret bearer-token");
        },
      });
      const response = await handlerFetch(app, "/private/repository", {
        headers: { "Graft-Protocol": "1" },
      });
      expect(response.status).toBe(500);
    } finally {
      console.error = originalError;
    }

    const logged = messages.join("\n");
    expect(logged).toContain("graft remote error reporter failed");
    expect(logged).toContain("unhandled graft remote error");
    expect(logged).not.toContain("storage-secret");
    expect(logged).not.toContain("repository/path");
    expect(logged).not.toContain("reporter-secret");
    expect(logged).not.toContain("bearer-token");
  });
});

async function handlerFetch(
  handler: ReturnType<typeof createTestApp>,
  path: string,
  init: RequestInit = {},
): Promise<Response> {
  const request = new Request(`${ORIGIN}${path}`, init);
  return await handler({
    request,
    route: routeParameters(request.url),
    adapterContext: undefined,
  });
}

function routeParameters(url: string): {
  namespace?: string;
  repository?: string;
  operation?: string;
  objectPath?: string;
} {
  const segments = new URL(url).pathname.slice(1).split("/").map(decodeURIComponent);
  const [namespace, repository, operation, ...objectSegments] = segments;
  return {
    ...(namespace === undefined ? {} : { namespace }),
    ...(repository === undefined ? {} : { repository }),
    ...(operation === undefined ? {} : { operation }),
    ...(objectSegments.length === 0 ? {} : { objectPath: objectSegments.join("/") }),
  };
}

async function bodyBytes(body: GraftWriteBody): Promise<Uint8Array<ArrayBuffer>> {
  if (body instanceof Uint8Array) {
    return body.slice();
  }
  const bytes = await new Response(body).arrayBuffer();
  return new Uint8Array(bytes);
}

function expectedMatches(
  current: Uint8Array<ArrayBuffer> | undefined,
  expected: Uint8Array<ArrayBuffer> | undefined,
): boolean {
  if (current === undefined || expected === undefined) {
    return current === expected;
  }
  return bytesEqual(current, expected);
}

function receivePackHeaders(
  packId: string,
  expected: string | undefined,
  replacement: string,
  packBytes: number,
  indexBytes: number,
  contentLength = packBytes + indexBytes,
): Headers {
  return new Headers({
    "content-length": contentLength.toString(),
    "x-graft-expected-present": (expected !== undefined).toString(),
    "x-graft-expected-hex": expected === undefined ? "" : textHex(expected),
    "x-graft-index-bytes": indexBytes.toString(),
    "x-graft-pack-bytes": packBytes.toString(),
    "x-graft-pack-id": packId,
    "x-graft-ref-replacement-hex": textHex(replacement),
  });
}

function receiveBundleHeaders(
  packId: string,
  expected: string | undefined,
  replacement: string,
  manifest: Uint8Array,
  packBytes: number,
  indexBytes: number,
  contentLength: number,
): Headers {
  const headers = receivePackHeaders(
    packId,
    expected,
    replacement,
    packBytes,
    indexBytes,
    contentLength,
  );
  headers.set("x-graft-bundle-manifest-bytes", manifest.byteLength.toString());
  return headers;
}

function joinBytes(parts: Uint8Array[]): Uint8Array<ArrayBuffer> {
  const bytes = new Uint8Array(
    new ArrayBuffer(parts.reduce((total, part) => total + part.length, 0)),
  );
  let offset = 0;
  for (const part of parts) {
    bytes.set(part, offset);
    offset += part.length;
  }
  return bytes;
}

function decodeUploadBundleFrames(bytes: Uint8Array, count: number): Array<[string, string]> {
  const frames: Array<[string, string]> = [];
  let offset = 0;
  for (let index = 0; index < count; index += 1) {
    const view = new DataView(bytes.buffer, bytes.byteOffset + offset);
    const pathBytes = view.getUint32(0);
    const bodyBytes = Number(view.getBigUint64(4));
    offset += 12;
    const path = new TextDecoder().decode(bytes.subarray(offset, offset + pathBytes));
    offset += pathBytes;
    const body = new TextDecoder().decode(bytes.subarray(offset, offset + bodyBytes));
    offset += bodyBytes;
    frames.push([path, body]);
  }
  expect(offset).toBe(bytes.byteLength);
  return frames;
}

function textHex(value: string): string {
  return [...new TextEncoder().encode(value)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}
