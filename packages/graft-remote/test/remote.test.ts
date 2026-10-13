import { describe, expect, it } from "vitest";
import readBundleVector from "../../../tests/fixtures/e2ee/read-bundle.json";

import {
  GraftProtocolError,
  bytewiseCompare,
  bytesEqual,
  createGraftRemoteHandler,
  type GraftByteRange,
  type GraftListQuery,
  type GraftListResult,
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

  list(query: GraftListQuery): GraftListResult {
    const matching = [...this.objects.keys()]
      .filter(
        (path) =>
          path.startsWith(query.prefix) &&
          (query.after === undefined || bytewiseCompare(path, query.after) > 0),
      )
      .sort(bytewiseCompare);
    const paths = matching.slice(0, query.limit);
    return {
      paths,
      hasMore: matching.length > query.limit,
      entries: paths.map((path) => ({ path, size: this.objects.get(path)!.byteLength })),
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
  it.each([true, false])("validates bundle framing with large streaming objects (%s)", async (complete) => {
    const backend = new MemoryRepository();
    const app = createGraftRemoteHandler({ authenticate() {}, backend: () => backend });
    const payload = new Uint8Array(65537).fill(42);
    const manifest = new TextEncoder().encode(JSON.stringify({ version: 1, objects: [
      {path: "segments/large", bytes: payload.length, allow_existing: true},
    ] }));
    const body = joinBytes([manifest, payload, new TextEncoder().encode(complete ? "pi" : "p")]);
    const response = await remoteFetch(app, "/test/large/receive-bundle/refs/heads/main", {
      method: "POST", headers: receiveBundleHeaders("e".repeat(64), undefined, "next\n", manifest, 1, 1, manifest.length + payload.length + 2), body,
    });
    expect(response.status).toBe(complete ? 204 : 400);
    expect(backend.objects.has("refs/heads/main")).toBe(complete);
    if (complete) expect(backend.objects.get("segments/large")).toEqual(payload);
  });

  it.each([false, true])("settles bounded bundle writes before publishing or failing (%s)", async (fail) => {
    const backend = new MemoryRepository();
    const put = backend.putIfAbsent.bind(backend);
    let active = 0;
    let peak = 0;
    backend.putIfAbsent = async (path, body) => {
      active++;
      peak = Math.max(peak, active);
      await new Promise(resolve => setTimeout(resolve, 10));
      try {
        if (fail && path === "segments/0") throw new Error("write failed");
        return await put(path, body);
      } finally { active--; }
    };
    const cas = backend.compareAndSwap.bind(backend);
    backend.compareAndSwap = (...args) => {
      expect(active).toBe(0);
      expect(backend.objects.size).toBe(8);
      return cas(...args);
    };
    const app = createGraftRemoteHandler({ authenticate() {}, backend: () => backend });
    const manifest = new TextEncoder().encode(JSON.stringify({ version: 1, objects:
      Array.from({length: 6}, (_, i) => ({path: `segments/${i}`, bytes: 1, allow_existing: true}))
    }));
    const body = joinBytes([manifest, new TextEncoder().encode("123456pi")]);
    const response = await remoteFetch(app, "/test/batch/receive-bundle/refs/heads/main", {
      method: "POST", headers: receiveBundleHeaders("d".repeat(64), undefined, "next\n", manifest, 1, 1, body.byteLength), body,
    });
    expect(response.status).toBe(fail ? 500 : 204);
    expect(active).toBe(0);
    expect(peak).toBe(4);
    expect(backend.objects.has("refs/heads/main")).toBe(!fail);
  });

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
        "read-bundle",
        "fetch-bundle",
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
    expect(Number(response.headers.get("content-length"))).toBe(bytes.byteLength);
    expect(Number(response.headers.get("x-graft-bundle-total-bytes"))).toBe(bytes.byteLength);
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

  it("reads an explicit set of objects through one read-bundle request", async () => {
    const app = createTestApp();
    for (const [path, body] of [
      ["objects/pack/one.idx", "index"],
      ["logs/example/commits/0000000000000001", "commit"],
    ] as const) {
      expect(
        (
          await remoteFetch(app, `/batch/repo/raw-if-not-exists/${path}`, {
            method: "PUT",
            body,
          })
        ).status,
      ).toBe(204);
    }

    const paths = [
      "logs/example/commits/0000000000000001",
      "objects/pack/one.idx",
    ];
    const response = await remoteFetch(app, "/batch/repo/read-bundle", {
      method: "POST",
      body: JSON.stringify({ version: 1, paths }),
    });
    expect(response.status, await response.clone().text()).toBe(200);
    expect(response.headers.get("content-type")).toBe("application/vnd.graft.read-bundle");
    expect(response.headers.get("x-graft-bundle-objects")).toBe("2");
    const bytes = new Uint8Array(await response.arrayBuffer());
    expect(Number(response.headers.get("content-length"))).toBe(bytes.byteLength);
    expect(decodeUploadBundleFrames(bytes, paths.length)).toEqual([
      [paths[0], "commit"],
      [paths[1], "index"],
    ]);

    const missing = await remoteFetch(app, "/batch/repo/read-bundle", {
      method: "POST",
      body: JSON.stringify({ version: 1, paths: ["objects/missing"] }),
    });
    expect(missing.status).toBe(404);

    for (const paths of [
      ["objects/pack/one.idx", "objects/pack/one.idx"],
      ["refs/heads/main"],
    ]) {
      const invalid = await remoteFetch(app, "/batch/repo/read-bundle", {
        method: "POST",
        body: JSON.stringify({ version: 1, paths }),
      });
      expect(invalid.status).toBe(400);
    }
  });

  it("streams only packs reachable between have and the ref in one fetch-bundle request", async () => {
    const app = createTestApp();
    const base = "1".repeat(64);
    const middle = "2".repeat(64);
    const head = "3".repeat(64);
    const unrelated = "4".repeat(64);
    const indexes = [
      ["base", base, []],
      ["middle", middle, [base]],
      ["head", head, [middle]],
      ["side", unrelated, []],
    ] as const;
    expect(
      (
        await remoteFetch(app, "/fetch/repo/raw/refs/heads/main", {
          method: "PUT",
          body: `${head}\n`,
        })
      ).status,
    ).toBe(204);
    for (const [name, id, parents] of indexes) {
      const index = JSON.stringify({
        version: 1,
        pack: `objects/pack/${name}.pack`,
        objects: [{ id, offset: 21, len: 1 }],
        commits: [{ id, parents }],
      });
      for (const [path, body] of [
        [`objects/pack/${name}.idx`, index],
        [`objects/pack/${name}.pack`, `pack-${name}`],
      ] as const) {
        expect(
          (
            await remoteFetch(app, `/fetch/repo/raw-if-not-exists/${path}`, {
              method: "PUT",
              body,
            })
          ).status,
        ).toBe(204);
      }
    }

    const response = await remoteFetch(app, "/fetch/repo/fetch-bundle/refs/heads/main", {
      method: "POST",
      body: JSON.stringify({ version: 1, have: base }),
    });
    expect(response.status, await response.clone().text()).toBe(200);
    expect(response.headers.get("content-type")).toBe("application/vnd.graft.fetch-bundle");
    const bytes = new Uint8Array(await response.arrayBuffer());
    const manifestBytes = Number(response.headers.get("x-graft-bundle-manifest-bytes"));
    const manifest = JSON.parse(new TextDecoder().decode(bytes.subarray(0, manifestBytes))) as {
      reference: { path: string; value_hex: string };
      objects: number;
    };
    expect(manifest.reference).toEqual({
      path: "refs/heads/main",
      value_hex: textHex(`${head}\n`),
    });
    expect(decodeUploadBundleFrames(bytes.subarray(manifestBytes), manifest.objects)).toEqual([
      ["objects/pack/head.idx", expect.any(String)],
      ["objects/pack/head.pack", "pack-head"],
      ["objects/pack/middle.idx", expect.any(String)],
      ["objects/pack/middle.pack", "pack-middle"],
    ]);
  });

  it("falls back when fetch-bundle ancestry hints are unavailable", async () => {
    const app = createTestApp();
    const head = "3".repeat(64);
    expect(
      (
        await remoteFetch(app, "/legacy-fetch/repo/raw/refs/heads/main", {
          method: "PUT",
          body: `${head}\n`,
        })
      ).status,
    ).toBe(204);
    expect(
      (
        await remoteFetch(app, "/legacy-fetch/repo/raw-if-not-exists/objects/pack/old.idx", {
          method: "PUT",
          body: JSON.stringify({
            version: 1,
            pack: "objects/pack/old.pack",
            objects: [],
          }),
        })
      ).status,
    ).toBe(204);

    const response = await remoteFetch(
      app,
      "/legacy-fetch/repo/fetch-bundle/refs/heads/main",
      { method: "POST", body: JSON.stringify({ version: 1, have: null }) },
    );
    expect(response.status).toBe(422);
    expect(await response.json()).toMatchObject({ title: "fetch_bundle_unavailable" });
  });

  it("calculates an exact upload-bundle size for path-only list backends", async () => {
    class PathOnlyListRepository extends MemoryRepository {
      headCalls = 0;

      override head(path: string): GraftObjectMetadata | null {
        this.headCalls += 1;
        return super.head(path);
      }

      override list(query: GraftListQuery): GraftListResult {
        const { paths, hasMore } = super.list(query);
        return { paths, hasMore };
      }
    }

    const backend = new PathOnlyListRepository();
    backend.put("refs/heads/main", new TextEncoder().encode("commit-1\n"));
    backend.put("objects/pack/one.idx", new TextEncoder().encode("index"));
    backend.put("objects/pack/one.pack", new TextEncoder().encode("pack-data"));
    const app = createGraftRemoteHandler({ backend: () => backend });

    const response = await remoteFetch(app, "/legacy/repo/upload-bundle/refs/heads/main", {
      method: "POST",
    });
    expect(response.status, await response.clone().text()).toBe(200);
    const bytes = new Uint8Array(await response.arrayBuffer());
    expect(Number(response.headers.get("x-graft-bundle-total-bytes"))).toBe(bytes.byteLength);
    expect(backend.headCalls).toBe(2);
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

describe("encrypted remote boundary", () => {
  const identity = {
    profile: "encrypted-remote-v1",
    version: 1,
    suite: 1,
    uuid: "123e4567-e89b-42d3-a456-426614174000",
  } as const;
  const pack = `e2ee/packs/${"a".repeat(64)}`;
  const ref = `e2ee/refs/${"b".repeat(64)}`;
  function setup(limits?: { maxRequestBytes: number }) {
    const backend = Object.assign(new MemoryRepository(), { encryption: identity });
    const app = createGraftRemoteHandler({ backend: () => backend, limits: limits === undefined ? undefined : {...limits, multipartPartBytes: limits.maxRequestBytes} });
    const call = (
      path: string,
      method = "GET",
      body?: string | Uint8Array<ArrayBuffer>,
      extra: Record<string, string> = {},
    ) =>
      handlerFetch(app, `/alice/repo${path}`, {
        method,
        body,
        headers: {
          "Graft-Protocol": "1",
          ...(body === undefined
            ? {}
            : {
                "Content-Length": String(
                  typeof body === "string" ? new TextEncoder().encode(body).length : body.length,
                ),
              }),
          ...extra,
        },
      });
    return { backend, call, app };
  }
  function objectFrame(): Uint8Array<ArrayBuffer> {
    const bytes = new Uint8Array(104);
    bytes.set(new TextEncoder().encode("GRAFTEO1"));
    bytes[9] = 1;
    bytes[11] = 1;
    new DataView(bytes.buffer).setUint32(68, 16);
    return bytes;
  }
  function configFrame(): Uint8Array<ArrayBuffer> {
    const bytes = new Uint8Array(206);
    bytes.set(new TextEncoder().encode("GRAFTEC1"));
    bytes[9] = 1;
    bytes[11] = 1;
    bytes[35] = 1;
    bytes[69] = 1;
    const raw = identity.uuid.replaceAll("-", "");
    for (let i = 0; i < 16; i += 1)
      bytes[12 + i] = Number.parseInt(raw.slice(i * 2, i * 2 + 2), 16);
    return bytes;
  }
  function refFrame(): Uint8Array<ArrayBuffer> {
    const bytes = new Uint8Array(100);
    bytes.set(new TextEncoder().encode("GRAFTER1"));
    bytes[9] = 1;
    bytes[11] = 1;
    bytes[12] = 1;
    return bytes;
  }
  function expected(value?: Uint8Array): Record<string, string> {
    return {
      "x-graft-expected-present": value === undefined ? "false" : "true",
      "x-graft-expected-hex":
        value === undefined
          ? ""
          : Array.from(value, (b) => b.toString(16).padStart(2, "0")).join(""),
    };
  }

  it("advertises only implemented encrypted capabilities", async () => {
    const { call } = setup();
    const response = await call("");
    expect(response.status).toBe(200);
    const value = await response.json();
    expect(value.encryption).toEqual(identity);
    expect(value.capabilities).toEqual([
      "range",
      "list",
      "list-cursor",
      "put-if-absent",
      "cas",
      "cad",
      "e2ee-read-bundle",
      "e2ee-fetch-bundle",
      "e2ee-receive-bundle",
    ]);
    expect(value.limits.multipart_part_bytes).toBeUndefined();
  });
  it("rejects plaintext keys, unconditional writes and semantic bypasses before mutation", async () => {
    const { call, backend } = setup();
    for (const operation of ["raw", "raw-if-not-exists", "cas", "cad"]) {
      expect(
        (
          await call(
            `/${operation}/refs/heads/main`,
            operation === "raw" ? "GET" : operation === "raw-if-not-exists" ? "PUT" : "POST",
          )
        ).status,
      ).toBe(400);
    }
    for (const operation of [
      "read-bundle",
      "fetch-bundle",
      "receive-bundle",
      "receive-pack",
      "upload-bundle",
    ]) {
      expect(
        (await call(`/${operation}${operation === "read-bundle" ? "" : `/${ref}`}`, "POST")).status,
      ).toBe(409);
    }
    expect((await call(`/multipart-start/${ref}`, "POST")).status).toBe(400);
    expect((await call(`/multipart-start/${pack}`, "POST")).status).toBe(404);
    expect((await call(`/raw/${pack}`, "PUT", objectFrame())).status).toBe(405);
    expect((await call(`/raw/${pack}`, "DELETE")).status).toBe(405);
    expect(backend.objects.size).toBe(0);
  });
  it("creates immutable ciphertext, reads ranges, and rejects replacement/CAS", async () => {
    const { call } = setup();
    expect((await call(`/raw-if-not-exists/${pack}`, "PUT", objectFrame())).status).toBe(204);
    expect((await call(`/raw-if-not-exists/${pack}`, "PUT", objectFrame())).status).toBe(412);
    const range = await call(`/raw/${pack}`, "GET", undefined, { Range: "bytes=0-7" });
    expect(range.status).toBe(206);
    expect(await range.text()).toBe("GRAFTEO1");
    expect((await call(`/cas/${pack}`, "POST", objectFrame(), expected())).status).toBe(400);
  });
  it("rejects malformed object headers and chunked uploads beyond declared limits", async () => {
    const { call, backend, app } = setup({ maxRequestBytes: 104 });
    const bad = objectFrame();
    bad[0] = bad[0]! ^ 1;
    expect((await call(`/raw-if-not-exists/${pack}`, "PUT", bad)).status).toBe(400);
    const oversize = new Uint8Array(105);
    oversize.set(objectFrame());
    expect((await call(`/raw-if-not-exists/${pack}`, "PUT", oversize)).status).toBe(413);
    const wrongLength = objectFrame();
    expect(
      (await call(`/raw-if-not-exists/${pack}`, "PUT", wrongLength, { "Content-Length": "103" }))
        .status,
    ).toBe(413);
    expect(
      (await call(`/raw-if-not-exists/${pack}`, "PUT", wrongLength, { "Content-Length": "105" }))
        .status,
    ).toBe(413);
    const noLength = await handlerFetch(app, `/alice/repo/raw-if-not-exists/${pack}`, {
      method: "PUT",
      body: objectFrame(),
      headers: { "Graft-Protocol": "1" },
    });
    expect(noLength.status).toBe(411);
    expect(backend.objects.size).toBe(0);
  });
  it("binds config mutations to UUID and preserves exact ref CAS/CAD", async () => {
    const { call, backend } = setup();
    const config = configFrame();
    const foreign = config.slice();
    foreign[12] = foreign[12]! ^ 1;
    expect((await call("/raw-if-not-exists/e2ee/config", "PUT", foreign)).status).toBe(400);
    expect((await call("/raw-if-not-exists/e2ee/config", "PUT", config)).status).toBe(204);
    expect((await call("/cad/e2ee/config", "POST", undefined, expected(foreign))).status).toBe(409);
    const original = refFrame();
    expect((await call(`/raw-if-not-exists/${ref}`, "PUT", original)).status).toBe(204);
    const replacement = original.slice();
    replacement[45] = 1;
    const [first, second] = await Promise.all([
      call(`/cas/${ref}`, "POST", replacement, expected(original)),
      call(`/cas/${ref}`, "POST", original, expected(replacement)),
    ]);
    // These are sequentially compatible transitions; verify the exact stale observation separately.
    expect(first.status).toBe(204);
    expect([204, 409]).toContain(second.status);
    backend.objects.set(ref, replacement);
    expect((await call(`/cad/${ref}`, "POST", undefined, expected(original))).status).toBe(409);
    expect((await call(`/cad/${ref}`, "POST", undefined, expected(replacement))).status).toBe(204);
    expect(backend.objects.has(ref)).toBe(false);
    const symbolic = original.slice();
    symbolic[12] = 2;
    expect((await call(`/raw-if-not-exists/${ref}`, "PUT", symbolic)).status).toBe(400);
  });
  it("preserves annotated-tag route envelopes and rejects wrong classes or unknown kinds", async () => {
    const { call, backend } = setup();
    const tag = refFrame();
    tag[12] = 3;
    expect((await call(`/raw-if-not-exists/${ref}`, "PUT", tag)).status).toBe(204);
    expect(backend.objects.get(ref)).toEqual(tag);
    const replacement = tag.slice();
    replacement[45] = 9;
    expect((await call(`/cas/${ref}`, "POST", replacement, expected(tag))).status).toBe(204);
    expect((await call(`/cad/${ref}`, "POST", undefined, expected(tag))).status).toBe(409);
    expect((await call(`/cad/${ref}`, "POST", undefined, expected(replacement))).status).toBe(204);
    const head = `e2ee/head/${"c".repeat(64)}`;
    expect((await call(`/raw-if-not-exists/${head}`, "PUT", tag)).status).toBe(400);
    const unknown = tag.slice(); unknown[12] = 4;
    expect((await call(`/raw-if-not-exists/${ref}`, "PUT", unknown)).status).toBe(400);
    expect(backend.objects.size).toBe(0);
  });
  it("paginates opaque names and rejects mixed or nonprogressing lists", async () => {
    const { call, backend } = setup();
    backend.objects.set(pack, objectFrame());
    backend.objects.set(ref, refFrame());
    const page = await (await call("/list?prefix=e2ee/&limit=1")).json();
    expect(page.paths).toHaveLength(1);
    const next = await (await call(`/list?cursor=${page.next_cursor}&limit=1`)).json();
    expect(next.paths).toHaveLength(1);
    expect(next.paths[0]).not.toBe(page.paths[0]);
    expect((await call("/list?prefix=objects/")).status).toBe(400);
    backend.objects.set("objects/plain", new Uint8Array());
    expect((await call("/list")).status).toBe(500);
  });
  it("returns sorted exact aggregate framing and excludes mutable objects", async () => {
    const { call, backend } = setup();
    const other = `e2ee/objects/${"c".repeat(64)}`;
    backend.objects.set(pack, objectFrame());
    backend.objects.set(other, objectFrame());
    const response = await call(
      "/e2ee/read-bundle",
      "POST",
      JSON.stringify({ version: 1, keys: [pack, other] }),
    );
    expect(response.status).toBe(200);
    const bytes = new Uint8Array(await response.arrayBuffer());
    expect(new TextDecoder().decode(bytes.subarray(0, 8))).toBe("GRAFTEB1");
    expect(Number(response.headers.get("content-length"))).toBe(bytes.length);
    expect(response.headers.get("x-graft-bundle-objects")).toBe("2");
    expect(response.headers.get("x-graft-bundle-total-bytes")).toBe(String(bytes.length));
    const view = new DataView(bytes.buffer);
    const fields = (input: Uint8Array): Uint8Array[] => {
      const data = new DataView(input.buffer, input.byteOffset, input.byteLength);
      const count = data.getUint16(0);
      let offset = 2;
      const values: Uint8Array[] = [];
      for (let i = 0; i < count; i += 1) {
        const size = data.getUint32(offset);
        offset += 4;
        values.push(input.subarray(offset, offset + size));
        offset += size;
      }
      expect(offset).toBe(input.length);
      return values;
    };
    const manifestLength = view.getUint32(8);
    const manifest = fields(bytes.subarray(12, 12 + manifestLength));
    expect([...manifest[0]!]).toEqual([0, 1]);
    expect(manifest[1]!.length).toBe(0);
    expect(manifest[2]!.length).toBe(0);
    const descriptors = fields(manifest[3]!);
    let offset = 12 + manifestLength;
    for (const descriptor of descriptors) {
      const [name, size] = fields(descriptor);
      const keyLength = view.getUint32(offset);
      const length = Number(view.getBigUint64(offset + 4));
      offset += 12;
      expect(bytes.subarray(offset, offset + keyLength)).toEqual(name);
      offset += keyLength;
      expect(length).toBe(
        Number(new DataView(size!.buffer, size!.byteOffset, size!.byteLength).getBigUint64(0)),
      );
      expect(bytes.subarray(offset, offset + length)).toEqual(objectFrame());
      offset += length;
    }
    expect(offset).toBe(bytes.length);
    expect(new TextDecoder().decode(fields(descriptors[0]!)[0])).toBe(other);
    for (const keys of [[ref], ["e2ee/config"], [pack, pack], [`e2ee/objects/${"d".repeat(64)}`]]) {
      expect(
        (await call("/e2ee/read-bundle", "POST", JSON.stringify({ version: 1, keys }))).status,
      ).not.toBe(200);
    }
  });
  it("matches the independently generated shared Rust/TypeScript framing vector", async () => {
    const fixture = readBundleVector;
    const {call, backend} = setup();
    backend.objects.set(fixture.key, new Uint8Array(fixture.payload_hex.match(/../g)!.map(byte => Number.parseInt(byte, 16))));
    const response = await call("/e2ee/read-bundle", "POST", JSON.stringify({version:1, keys:[fixture.key]}));
    expect(response.status).toBe(200);
    expect(Array.from(new Uint8Array(await response.arrayBuffer()), byte => byte.toString(16).padStart(2, "0")).join("")).toBe(fixture.wire_hex);
  });
  it("rejects duplicate/unknown JSON fields, invalid lengths and excessive aggregates", async () => {
    const { call, backend } = setup();
    backend.objects.set(pack, objectFrame());
    for (const body of [
      `{"version":1,"version":1,"keys":["${pack}"]}`,
      `{"version":1,"\\u0076ersion":1,"keys":["${pack}"]}`,
      JSON.stringify({ version: 1, keys: [pack], extra: 1 }),
    ]) {
      expect((await call("/e2ee/read-bundle", "POST", body)).status).toBe(400);
    }
    const body = JSON.stringify({ version: 1, keys: [pack] });
    expect((await call("/e2ee/read-bundle", "POST", body, { "Content-Length": "1" })).status).toBe(
      400,
    );
    backend.head = () => ({ size: 64 * 1024 * 1024 });
    expect((await call("/e2ee/read-bundle", "POST", body)).status).toBe(413);
  });
  it("cancels open aggregate sources and rejects truncated bodies", async () => {
    const {call, backend} = setup();
    backend.head = () => ({size:104});
    let cancelled = 0;
    backend.get = () => ({size:104, body:new ReadableStream<Uint8Array>({pull(controller) {controller.enqueue(new Uint8Array(1));}, cancel() {cancelled += 1;}})});
    const response = await call("/e2ee/read-bundle", "POST", JSON.stringify({version:1,keys:[pack]}));
    const reader = response.body!.getReader();
    await reader.read(); // Manifest.
    await reader.read(); // Frame header; source has been opened.
    await reader.cancel();
    expect(cancelled).toBe(1);
    backend.get = () => ({size:104, body:new Uint8Array(103)});
    const truncated = await call("/e2ee/read-bundle", "POST", JSON.stringify({version:1,keys:[pack]}));
    await expect(truncated.arrayBuffer()).rejects.toThrow();
  });
  it("validates public hint identity, key association and topology before creation", async () => {
    const {call, backend} = setup();
    const hint = pack.replace("e2ee/packs/", "e2ee/hints/");
    const encode = (fields: Uint8Array[]): Uint8Array<ArrayBuffer> => {
      const result = new Uint8Array(2 + fields.reduce((size, field) => size + 4 + field.length, 0));
      const view = new DataView(result.buffer); view.setUint16(0, fields.length); let offset = 2;
      for (const field of fields) {view.setUint32(offset, field.length); offset += 4; result.set(field, offset); offset += field.length;}
      return result;
    };
    const encoder = new TextEncoder();
    const build = (entries: Uint8Array[]): Uint8Array<ArrayBuffer> => {
      const body = encode([encoder.encode(pack), encoder.encode(`e2ee/indexes/${"c".repeat(64)}`), encode(entries)]);
      const frame = new Uint8Array(28 + body.length + 32);
      frame.set(encoder.encode("GRAFTEH1")); frame[9] = 1; frame[11] = 1;
      const uuid = identity.uuid.replaceAll("-", "");
      for (let i = 0; i < 16; i += 1) frame[12 + i] = Number.parseInt(uuid.slice(i * 2, i * 2 + 2), 16);
      frame.set(body, 28); return frame;
    };
    const first = encode([new Uint8Array(32).fill(1), encode([])]);
    const second = encode([new Uint8Array(32).fill(2), encode([new Uint8Array(32).fill(1)])]);
    const duplicateParent = encode([new Uint8Array(32).fill(3), encode([new Uint8Array(32).fill(1), new Uint8Array(32).fill(1)])]);
    for (const entries of [[first, first], [second, first], [duplicateParent]]) {
      expect((await call(`/raw-if-not-exists/${hint}`, "PUT", build(entries))).status).toBe(400);
    }
    const valid = build([first, second]);
    const foreign = valid.slice(); foreign[12] = foreign[12]! ^ 1;
    expect((await call(`/raw-if-not-exists/${hint}`, "PUT", foreign)).status).toBe(400);
    expect((await call(`/raw-if-not-exists/${hint.replace(/a/g, "d")}`, "PUT", valid)).status).toBe(400);
    expect(backend.objects.size).toBe(0);
    // The server validates public shape, never claims to validate this synthetic MAC.
    expect((await call(`/raw-if-not-exists/${hint}`, "PUT", valid)).status).toBe(204);
    expect(backend.objects.get(hint)).toEqual(valid);
  });
  it("authenticates before any encrypted backend work", async () => {
    let backendCalls = 0;
    const app = createGraftRemoteHandler({
      authenticate: () => {
        throw new GraftProtocolError(401, "unauthorized", "Denied");
      },
      backend: () => {
        backendCalls += 1;
        return null;
      },
    });
    expect(
      (
        await handlerFetch(app, "/alice/repo/e2ee/read-bundle", {
          method: "POST",
          headers: { "Graft-Protocol": "1" },
        })
      ).status,
    ).toBe(401);
    expect(backendCalls).toBe(0);
  });
  it("rejects encrypted aggregate routes on a plain backend", async () => {
    const app = createGraftRemoteHandler({ backend: () => new MemoryRepository() });
    expect(
      (
        await handlerFetch(app, "/alice/repo/e2ee/read-bundle", {
          method: "POST",
          headers: { "Graft-Protocol": "1" },
        })
      ).status,
    ).toBe(409);
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
