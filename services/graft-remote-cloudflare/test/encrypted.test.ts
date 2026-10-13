import { abortAllDurableObjects, env, SELF } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import {
  CloudflareRepositoryBackend,
  createEncryptedCloudflareRepository,
  openCloudflareRepository,
  rebuildCloudflareEncryptedRouting,
} from "@eidos.space/graft-remote-cloudflare";

const uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
const uploadId = "a".repeat(64);
const key = `e2ee/files/${"b".repeat(64)}`;
const storage = () => ({ objects: env.OBJECTS, repositories: env.REPOSITORIES });
const hex = (bytes: Uint8Array) => [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
function signal() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
function object(length = 104): Uint8Array<ArrayBuffer> {
  const bytes = new Uint8Array(length).fill(7);
  bytes.set(new TextEncoder().encode("GRAFTEO1"));
  bytes.set([0, 1, 0, 1], 8);
  bytes.fill(0xaa, 12, 44);
  new DataView(bytes.buffer).setUint32(68, 16);
  return bytes;
}
async function call(
  id: string,
  operation = "",
  body?: Uint8Array<ArrayBuffer>,
  headers: Record<string, string> = {},
  method = body === undefined ? "GET" : "PUT",
) {
  return SELF.fetch(`https://remote.example/${id}${operation ? `/${operation}` : ""}`, {
    method,
    body,
    headers: {
      Authorization: "Bearer test-token",
      "Graft-Protocol": "1",
      ...(body === undefined ? {} : { "Content-Length": String(body.length) }),
      ...headers,
    },
  });
}
async function identity(bytes: Uint8Array<ArrayBuffer>) {
  return {
    "x-graft-e2ee-uuid": uuid,
    "x-graft-e2ee-upload-id": uploadId,
    "x-graft-e2ee-ciphertext-sha256": hex(
      new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
    ),
    "x-graft-object-bytes": String(bytes.length),
    "content-length": "0",
  };
}
async function start(
  id: string,
  bytes: Uint8Array<ArrayBuffer>,
  overrides: Record<string, string> = {},
) {
  return call(
    id,
    `multipart-start/${key}`,
    undefined,
    { ...(await identity(bytes)), ...overrides },
    "POST",
  );
}
async function part(id: string, session: string, bytes: Uint8Array<ArrayBuffer>) {
  return call(id, `multipart-part/${key}`, bytes, {
    "x-graft-upload-id": session,
    "x-graft-part-number": "1",
  });
}
async function complete(id: string, session: string) {
  return call(
    id,
    `multipart-complete/${key}`,
    undefined,
    { "x-graft-upload-id": session, "content-length": "0" },
    "POST",
  );
}

function concat(...parts: Uint8Array<ArrayBuffer>[]): Uint8Array<ArrayBuffer> {
  const value = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    value.set(part, offset);
    offset += part.length;
  }
  return value;
}
function integer(value: number, width: 2 | 4): Uint8Array<ArrayBuffer> {
  const bytes = new Uint8Array(width);
  if (width === 2) new DataView(bytes.buffer).setUint16(0, value);
  else new DataView(bytes.buffer).setUint32(0, value);
  return bytes;
}
function fields(...parts: Uint8Array<ArrayBuffer>[]): Uint8Array<ArrayBuffer> {
  return concat(
    integer(parts.length, 2),
    ...parts.flatMap((part) => [integer(part.length, 4), part]),
  );
}
function routingHint(pack: string, token: number, parents: number[]): Uint8Array<ArrayBuffer> {
  const encode = (value: string) => new TextEncoder().encode(value);
  const idBytes = Uint8Array.from(uuid.replaceAll("-", "").match(/../g)!, (pair) =>
    Number.parseInt(pair, 16),
  );
  return concat(
    encode("GRAFTEH1"),
    integer(1, 2),
    integer(1, 2),
    idBytes,
    fields(
      encode(`e2ee/packs/${pack.repeat(64)}`),
      encode(`e2ee/indexes/${pack.repeat(64)}`),
      fields(
        fields(
          new Uint8Array(32).fill(token),
          fields(...parents.map((parent) => new Uint8Array(32).fill(parent))),
        ),
      ),
    ),
    new Uint8Array(32),
  );
}

describe("encrypted Cloudflare storage", () => {
  it("retains incremental hint planning across eviction and rejects conflicting topology", async () => {
    const id = "encrypted/routing";
    await createEncryptedCloudflareRepository(storage(), id, uuid);
    for (const [pack, token, parents] of [
      ["c", 1, []],
      ["d", 2, [1]],
    ] as const) {
      for (const kind of ["packs", "indexes"])
        expect(
          (await call(id, `raw-if-not-exists/e2ee/${kind}/${pack.repeat(64)}`, object())).status,
        ).toBe(204);
      expect(
        (
          await call(
            id,
            `raw-if-not-exists/e2ee/hints/${pack.repeat(64)}`,
            routingHint(pack, token, [...parents]),
          )
        ).status,
      ).toBe(204);
    }
    const refKey = `e2ee/refs/${"c".repeat(64)}`;
    const reference = new Uint8Array(85);
    reference.set(new TextEncoder().encode("GRAFTER1"));
    reference.set([0, 1, 0, 1, 1], 8);
    reference.fill(2, 13, 45);
    expect(
      (
        await call(
          id,
          `cas/${refKey}`,
          reference,
          { "x-graft-expected-present": "false", "x-graft-expected-hex": "" },
          "POST",
        )
      ).status,
    ).toBe(204);
    await abortAllDurableObjects();
    const backend = await openCloudflareRepository(storage(), id);
    const selected = await backend.encryptedRouting!.select("02".repeat(32), "01".repeat(32));
    expect(selected).toEqual([
      {
        key: `e2ee/hints/${"d".repeat(64)}`,
        pack: `e2ee/packs/${"d".repeat(64)}`,
        index: `e2ee/indexes/${"d".repeat(64)}`,
      },
    ]);
    const request = new TextEncoder().encode(JSON.stringify({ version: 1, have: "01".repeat(32) }));
    const fetched = await call(id, `e2ee/fetch-bundle/${refKey}`, request, {}, "POST");
    expect(fetched.status, await fetched.clone().text()).toBe(200);
    expect(fetched.headers.get("x-graft-bundle-objects")).toBe("3");
    await fetched.arrayBuffer();
    await env.REPOSITORIES.getByName(id).resetEncryptedRouting();
    expect((await call(id, `e2ee/fetch-bundle/${refKey}`, request, {}, "POST")).status).toBe(422);
    expect(await rebuildCloudflareEncryptedRouting(storage(), id)).toBe(2);
    expect(await backend.encryptedRouting!.select("02".repeat(32), "01".repeat(32))).toEqual(
      selected,
    );
    expect(
      (await call(id, `raw-if-not-exists/e2ee/hints/${"e".repeat(64)}`, routingHint("e", 2, [])))
        .status,
    ).toBe(204);
    await abortAllDurableObjects();
    expect((await call(id, `e2ee/fetch-bundle/${refKey}`, request, {}, "POST")).status).toBe(422);
  });
  it("persists profile and rejects conversion or competing identities", async () => {
    const id = "encrypted/profile";
    await createEncryptedCloudflareRepository(storage(), id, uuid);
    await abortAllDurableObjects();
    const descriptor = await call(id);
    expect(await descriptor.json()).toMatchObject({
      encryption: { uuid },
      capabilities: expect.arrayContaining(["e2ee-multipart-object"]),
    });
    await expect(
      createEncryptedCloudflareRepository(storage(), id, "10010203-0405-4607-8809-0a0b0c0d0e0f"),
    ).rejects.toThrow();
    await expect(
      new CloudflareRepositoryBackend(storage(), id).put("HEAD", new Uint8Array([1])),
    ).rejects.toThrow();
    expect((await call(id, "raw/HEAD", new Uint8Array([1]))).status).toBe(400);
    const plain = "encrypted/existing-plain";
    await new CloudflareRepositoryBackend(storage(), plain).putIfAbsent(
      "objects/a",
      new Uint8Array([1]),
      "immutable",
    );
    await expect(createEncryptedCloudflareRepository(storage(), plain, uuid)).rejects.toThrow();
    const raced = await Promise.allSettled([
      createEncryptedCloudflareRepository(storage(), "encrypted/profile-race", uuid),
      createEncryptedCloudflareRepository(
        storage(),
        "encrypted/profile-race",
        "10010203-0405-4607-8809-0a0b0c0d0e0f",
      ),
    ]);
    expect(raced.filter((result) => result.status === "fulfilled")).toHaveLength(1);
  });

  it("routes encrypted mutable bytes through durable CAS storage", async () => {
    const id = "encrypted/refs";
    await createEncryptedCloudflareRepository(storage(), id, uuid);
    const refKey = `e2ee/refs/${"c".repeat(64)}`;
    const ref = new Uint8Array(85);
    ref.set(new TextEncoder().encode("GRAFTER1"));
    ref.set([0, 1, 0, 1, 1], 8);
    expect(
      (
        await call(
          id,
          `cas/${refKey}`,
          ref,
          { "x-graft-expected-present": "false", "x-graft-expected-hex": "" },
          "POST",
        )
      ).status,
    ).toBe(204);
    await abortAllDurableObjects();
    expect(new Uint8Array(await (await call(id, `raw/${refKey}`)).arrayBuffer())).toEqual(ref);
    expect(
      (
        await call(
          id,
          `cas/${refKey}`,
          ref,
          { "x-graft-expected-present": "false", "x-graft-expected-hex": "" },
          "POST",
        )
      ).status,
    ).toBe(409);
    const listing = await call(id, "list?prefix=e2ee%2Frefs%2F");
    expect(await listing.json()).toMatchObject({ paths: [refKey] });
  });

  it("resumes exact ciphertext across eviction and publishes only verified completion", async () => {
    const id = "encrypted/multipart";
    await createEncryptedCloudflareRepository(storage(), id, uuid);
    const bytes = object(6 * 1024 * 1024);
    const initial = await start(id, bytes);
    expect(initial.status, await initial.clone().text()).toBe(200);
    const session = (await initial.json()) as { upload_id: string };
    expect((await complete(id, session.upload_id)).status).toBe(400);
    expect((await part(id, session.upload_id, bytes)).status).toBe(204);
    expect((await call(id, `raw/${key}`)).status).toBe(404);
    await abortAllDurableObjects();
    const resumed = await start(id, bytes);
    expect(await resumed.json()).toMatchObject({
      upload_id: session.upload_id,
      encryption: { uuid, upload_id: uploadId },
      uploaded_parts: [{ part_number: 1, bytes: bytes.length }],
    });
    const conflictingIdentities: Record<string, string>[] = [
      { "x-graft-e2ee-upload-id": "c".repeat(64) },
      { "x-graft-e2ee-ciphertext-sha256": "d".repeat(64) },
      { "x-graft-object-bytes": String(bytes.length + 1) },
    ];
    for (const overrides of conflictingIdentities)
      expect((await start(id, bytes, overrides)).status).toBe(409);
    const completed = await complete(id, session.upload_id);
    expect(completed.status, await completed.clone().text()).toBe(204);
    expect((await complete(id, session.upload_id)).status).toBe(412);
    const result = await call(id, `raw/${key}`, undefined, { Range: "bytes=0-71" });
    expect(result.status).toBe(206);
    expect(new Uint8Array(await result.arrayBuffer())).toEqual(bytes.slice(0, 72));
    expect(await (await call(id, "list?prefix=e2ee%2F")).json()).toMatchObject({ paths: [key] });
  });

  it("does not publish a wrong ciphertext digest, upload identity or public frame", async () => {
    for (const fault of ["digest", "upload", "frame"]) {
      const id = `encrypted/bad-${fault}`;
      await createEncryptedCloudflareRepository(storage(), id, uuid);
      const bytes = object();
      if (fault === "frame") bytes[0] = 0;
      if (fault === "upload") bytes[12] = 0;
      const response = await start(
        id,
        bytes,
        fault === "digest" ? { "x-graft-e2ee-ciphertext-sha256": "f".repeat(64) } : {},
      );
      const session = (await response.json()) as { upload_id: string };
      expect((await part(id, session.upload_id, bytes)).status).toBe(204);
      expect((await complete(id, session.upload_id)).status).toBe(400);
      expect((await call(id, `raw/${key}`)).status).toBe(404);
      expect(await (await call(id, "list?prefix=e2ee%2F")).json()).toMatchObject({ paths: [] });
      expect(
        (
          await call(
            id,
            `multipart-abort/${key}`,
            undefined,
            { "x-graft-upload-id": session.upload_id, "content-length": "0" },
            "DELETE",
          )
        ).status,
      ).toBe(204);
      expect((await start(id, object())).status).toBe(200);
    }
  });

  it("shares create-only publication between direct and multipart writers", async () => {
    const id = "encrypted/collision";
    await createEncryptedCloudflareRepository(storage(), id, uuid);
    const bytes = object();
    const starts = await Promise.all([start(id, bytes), start(id, bytes)]);
    const sessions = await Promise.all(
      starts.map((response) => response.json() as Promise<{ upload_id: string }>),
    );
    expect(sessions[0]!.upload_id).toBe(sessions[1]!.upload_id);
    const session = sessions[0]!.upload_id;
    expect((await part(id, session, bytes)).status).toBe(204);
    const winner = object();
    winner[103] = 9;
    expect((await call(id, `raw-if-not-exists/${key}`, winner)).status).toBe(204);
    expect((await complete(id, session)).status).toBe(412);
    const opened = await openCloudflareRepository(storage(), id);
    expect(await opened.head(key)).toEqual({ size: bytes.length });
    expect(new Uint8Array(await (await call(id, `raw/${key}`)).arrayBuffer())).toEqual(winner);
  });

  it.each([false, true])("cannot publish after abort (competing verifier rejected: %s)", async (reject) => {
    const id = `encrypted/completion-abort-race-${reject}`;
    const entered = signal();
    const release = signal();
    let reads = 0;
    const objects = new Proxy(env.OBJECTS, {
      get(target, property) {
        if (property !== "get") {
          const value: unknown = Reflect.get(target, property, target);
          return typeof value === "function" ? value.bind(target) : value;
        }
        return async (path: string) => {
          const object = await target.get(path);
          if (object === null) return null;
          const bytes = new Uint8Array(await object.arrayBuffer());
          if (++reads === 1) {
            entered.resolve();
            await release.promise;
          } else bytes[bytes.length - 1]! ^= 1;
          return new Proxy(object, {
            get(target, property) {
              if (property === "body")
                return new ReadableStream<Uint8Array>({
                  start(controller) {
                    controller.enqueue(bytes);
                    controller.close();
                  },
                });
              return Reflect.get(target, property, target);
            },
          });
        };
      },
    });
    const backend = await createEncryptedCloudflareRepository(
      { objects, repositories: env.REPOSITORIES },
      id,
      uuid,
    );
    const bytes = object();
    const initial = (await (await start(id, bytes)).json()) as { upload_id: string };
    expect((await part(id, initial.upload_id, bytes)).status).toBe(204);
    const completing = Promise.resolve(backend.multipart!.complete(key, initial.upload_id));
    const result = completing.then(
      () => "published",
      () => "rejected",
    );
    await entered.promise;
    if (reject)
      await expect(backend.multipart!.complete(key, initial.upload_id)).rejects.toThrow();
    await backend.multipart!.abort(key, initial.upload_id);
    release.resolve();
    expect(await result).toBe("rejected");
    expect(await backend.head(key)).toBeNull();
    expect(await env.REPOSITORIES.getByName(id).getEncryptedUpload(key)).toBeNull();
  });

  it("does not freeze acknowledged parts while a replacement part is in flight", async () => {
    const id = "encrypted/part-freeze-race";
    const entered = signal();
    const release = signal();
    const objects = new Proxy(env.OBJECTS, {
      get(target, property) {
        if (property !== "resumeMultipartUpload") {
          const value: unknown = Reflect.get(target, property, target);
          return typeof value === "function" ? value.bind(target) : value;
        }
        return (path: string, upload: string) =>
          new Proxy(target.resumeMultipartUpload(path, upload), {
            get(target, property) {
              if (property === "uploadPart")
                return async (...args: Parameters<R2MultipartUpload["uploadPart"]>) => {
                  entered.resolve();
                  await release.promise;
                  return target.uploadPart(...args);
                };
              const value: unknown = Reflect.get(target, property, target);
              return typeof value === "function" ? value.bind(target) : value;
            },
          });
      },
    });
    const backend = await createEncryptedCloudflareRepository(
      { objects, repositories: env.REPOSITORIES },
      id,
      uuid,
    );
    const bytes = object();
    const initial = (await (await start(id, bytes)).json()) as { upload_id: string };
    expect((await part(id, initial.upload_id, bytes)).status).toBe(204);
    const writing = Promise.resolve(backend.multipart!.uploadPart(
      key,
      initial.upload_id,
      1,
      new Response(bytes).body!,
      bytes.length,
    ));
    await entered.promise;
    try {
      await expect(backend.multipart!.complete(key, initial.upload_id)).rejects.toThrow();
      expect(await backend.head(key)).toBeNull();
    } finally {
      release.resolve();
      await writing.catch(() => undefined);
    }
    await writing;
    expect(await backend.multipart!.complete(key, initial.upload_id)).toBe(true);
  });

  it("can explicitly abort a frozen session whose underlying R2 upload expired", async () => {
    const id = "encrypted/expired-upload";
    const backend = await createEncryptedCloudflareRepository(storage(), id, uuid);
    const bytes = object();
    const initial = (await (await start(id, bytes)).json()) as { upload_id: string };
    expect((await part(id, initial.upload_id, bytes)).status).toBe(204);
    const state = await env.REPOSITORIES.getByName(id).getEncryptedUpload(key);
    await env.OBJECTS.resumeMultipartUpload(state!.key, initial.upload_id).abort();
    await expect(backend.multipart!.complete(key, initial.upload_id)).rejects.toThrow();
    await backend.multipart!.abort(key, initial.upload_id);
    const restarted = (await (await start(id, bytes)).json()) as { upload_id: string };
    expect(restarted.upload_id).not.toBe(initial.upload_id);
    expect((await part(id, restarted.upload_id, bytes)).status).toBe(204);
    expect((await complete(id, restarted.upload_id)).status).toBe(204);
    await backend.multipart!.abort(key, initial.upload_id);
    expect(new Uint8Array(await (await call(id, `raw/${key}`)).arrayBuffer())).toEqual(bytes);
  });

  it("retains published ciphertext when its atomic publication acknowledgement is lost", async () => {
    const id = "encrypted/unknown-publication";
    const repositories = new Proxy(env.REPOSITORIES, {
      get(target, property) {
        if (property !== "getByName") {
          const value: unknown = Reflect.get(target, property, target);
          return typeof value === "function" ? value.bind(target) : value;
        }
        return (name: string) =>
          new Proxy(target.getByName(name), {
            get(target, property) {
              if (property === "publishEncryptedUpload")
                return async (...args: Parameters<typeof target.publishEncryptedUpload>) => {
                  await target.publishEncryptedUpload(...args);
                  throw new Error("Publication response lost");
                };
              if (property === "publishEncryptedObject")
                return async (...args: Parameters<typeof target.publishEncryptedObject>) => {
                  await target.publishEncryptedObject(...args);
                  throw new Error("Publication response lost");
                };
              const value: unknown = Reflect.get(target, property, target);
              return typeof value === "function"
                ? (...args: unknown[]) => Reflect.apply(value, target, args)
                : value;
            },
          });
      },
    });
    const backend = await createEncryptedCloudflareRepository(
      { objects: env.OBJECTS, repositories },
      id,
      uuid,
    );
    const bytes = object();
    const initial = (await (await start(id, bytes)).json()) as { upload_id: string };
    expect((await part(id, initial.upload_id, bytes)).status).toBe(204);
    await expect(backend.multipart!.complete(key, initial.upload_id)).rejects.toThrow(
      "Publication response lost",
    );
    await backend.multipart!.abort(key, initial.upload_id);
    await abortAllDurableObjects();
    expect(new Uint8Array(await (await call(id, `raw/${key}`)).arrayBuffer())).toEqual(bytes);
    const reopened = await openCloudflareRepository({ objects: env.OBJECTS, repositories }, id);
    expect(await reopened.multipart!.complete(key, initial.upload_id)).toBe(false);
    const direct = `e2ee/files/${"c".repeat(64)}`;
    await expect(reopened.putIfAbsent(direct, bytes, "immutable")).rejects.toThrow(
      "Publication response lost",
    );
    await abortAllDurableObjects();
    expect(new Uint8Array(await (await call(id, `raw/${direct}`)).arrayBuffer())).toEqual(bytes);
  });

  it("retains unknown part admission across eviction until explicit abort", async () => {
    const id = "encrypted/unknown-part";
    const objects = new Proxy(env.OBJECTS, {
      get(target, property) {
        if (property !== "resumeMultipartUpload") {
          const value: unknown = Reflect.get(target, property, target);
          return typeof value === "function" ? value.bind(target) : value;
        }
        return (path: string, upload: string) =>
          new Proxy(target.resumeMultipartUpload(path, upload), {
            get(target, property) {
              if (property === "uploadPart")
                return async (...args: Parameters<R2MultipartUpload["uploadPart"]>) => {
                  await target.uploadPart(...args);
                  throw new Error("Part response lost before durable acknowledgement");
                };
              const value: unknown = Reflect.get(target, property, target);
              return typeof value === "function" ? value.bind(target) : value;
            },
          });
      },
    });
    const backend = await createEncryptedCloudflareRepository(
      { objects, repositories: env.REPOSITORIES },
      id,
      uuid,
    );
    const bytes = object();
    const initial = (await (await start(id, bytes)).json()) as { upload_id: string };
    await expect(backend.multipart!.uploadPart(
      key, initial.upload_id, 1, new Response(bytes).body!, bytes.length,
    )).rejects.toThrow("Part response lost");
    await abortAllDurableObjects();
    expect(await (await start(id, bytes)).json()).toMatchObject({
      upload_id: initial.upload_id, uploaded_parts: [],
    });
    expect((await part(id, initial.upload_id, bytes)).status).toBe(409);
    expect((await complete(id, initial.upload_id)).status).toBe(400);
    const reopened = await openCloudflareRepository(storage(), id);
    expect(await reopened.head(key)).toBeNull();
    await reopened.multipart!.abort(key, initial.upload_id);
    const next = (await (await start(id, bytes)).json()) as { upload_id: string };
    expect(next.upload_id).not.toBe(initial.upload_id);
    expect((await part(id, next.upload_id, bytes)).status).toBe(204);
    expect((await complete(id, next.upload_id)).status).toBe(204);
  });

  it("rejects missing upload identity and multipart hints before writing", async () => {
    const id = "encrypted/invalid-multipart";
    await createEncryptedCloudflareRepository(storage(), id, uuid);
    expect(
      (
        await call(
          id,
          `multipart-start/${key}`,
          undefined,
          { "x-graft-object-bytes": "104", "content-length": "0" },
          "POST",
        )
      ).status,
    ).toBe(400);
    const headers = await identity(object());
    for (const invalid of [
      `e2ee/hints/${"b".repeat(64)}`,
      "e2ee/config",
      `e2ee/refs/${"b".repeat(64)}`,
    ])
      expect(
        (await call(id, `multipart-start/${invalid}`, undefined, headers, "POST")).status,
      ).toBe(400);
    expect(await (await call(id, "list?prefix=e2ee%2F")).json()).toMatchObject({ paths: [] });
  });
});
