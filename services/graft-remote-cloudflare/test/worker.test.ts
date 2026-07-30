import { abortAllDurableObjects, SELF } from "cloudflare:test";
import { describe, expect, it } from "vitest";

const ORIGIN = "https://remote.example";

function remoteFetch(
  path: string,
  init: RequestInit = {},
  options: { token?: string; protocol?: string } = {},
): Promise<Response> {
  const headers = new Headers(init.headers);
  headers.set("Authorization", `Bearer ${options.token ?? "test-token"}`);
  headers.set("Graft-Protocol", options.protocol ?? "1");
  return SELF.fetch(`${ORIGIN}${path}`, { ...init, headers });
}

async function responseBytes(response: Response): Promise<number[]> {
  return [...new Uint8Array(await response.arrayBuffer())];
}

async function responseText(response: Response): Promise<string> {
  return new TextDecoder().decode(await response.arrayBuffer());
}

function decodeUploadBundle(bytes: Uint8Array, manifestBytes: number): Array<[string, string]> {
  const manifest = JSON.parse(new TextDecoder().decode(bytes.subarray(0, manifestBytes))) as {
    objects: number;
  };
  const objects: Array<[string, string]> = [];
  let offset = manifestBytes;
  for (let index = 0; index < manifest.objects; index += 1) {
    const view = new DataView(bytes.buffer, bytes.byteOffset + offset);
    const pathBytes = view.getUint32(0);
    const bodyBytes = Number(view.getBigUint64(4));
    offset += 12;
    const path = new TextDecoder().decode(bytes.subarray(offset, offset + pathBytes));
    offset += pathBytes;
    const body = new TextDecoder().decode(bytes.subarray(offset, offset + bodyBytes));
    offset += bodyBytes;
    objects.push([path, body]);
  }
  expect(offset).toBe(bytes.byteLength);
  return objects;
}

describe("graft remote protocol", () => {
  it("requires authentication and protocol negotiation", async () => {
    const unauthorized = await SELF.fetch(`${ORIGIN}/auth/repo`, {
      headers: { "Graft-Protocol": "1" },
    });
    expect(unauthorized.status).toBe(401);
    expect(unauthorized.headers.get("www-authenticate")).toContain("Bearer");
    expect(unauthorized.headers.get("graft-protocol")).toBe("1");
    await unauthorized.body?.cancel();

    const unmatchedRoute = await SELF.fetch(`${ORIGIN}/not-a-repository`, {
      headers: { "Graft-Protocol": "1" },
    });
    expect(unmatchedRoute.status).toBe(404);
    await unmatchedRoute.body?.cancel();

    const unsupported = await remoteFetch("/auth/repo", {}, { protocol: "2" });
    expect(unsupported.status).toBe(426);
    expect(unsupported.headers.get("graft-protocol")).toBe("1");
    await unsupported.body?.cancel();

    const descriptor = await remoteFetch("/auth/repo");
    expect(descriptor.status).toBe(200);
    expect(await descriptor.json()).toMatchObject({
      protocol: "graft-remote",
      version: 1,
      repository: "auth/repo",
      capabilities: expect.arrayContaining(["list", "list-cursor"]),
    });
  });

  it("stores immutable objects with create-only semantics and streams ranges", async () => {
    const path = "/objects/repo/raw-if-not-exists/objects/pack/data.pack";
    const created = await remoteFetch(path, { method: "PUT", body: "abcdef" });
    expect(created.status).toBe(204);

    const collision = await remoteFetch(path, { method: "PUT", body: "changed" });
    expect(collision.status).toBe(412);
    await collision.body?.cancel();

    const full = await remoteFetch("/objects/repo/raw/objects/pack/data.pack");
    expect(full.status).toBe(200);
    expect(await responseText(full)).toBe("abcdef");

    const range = await remoteFetch("/objects/repo/raw/objects/pack/data.pack", {
      headers: { Range: "bytes=1-3" },
    });
    expect(range.status).toBe(206);
    expect(range.headers.get("content-range")).toBe("bytes 1-3/6");
    expect(await responseText(range)).toBe("bcd");

    const suffix = await remoteFetch("/objects/repo/raw/objects/pack/data.pack", {
      headers: { Range: "bytes=-2" },
    });
    expect(suffix.status).toBe(206);
    expect(await responseText(suffix)).toBe("ef");

    const unsatisfiable = await remoteFetch("/objects/repo/raw/objects/pack/data.pack", {
      headers: { Range: "bytes=99-100" },
    });
    expect(unsatisfiable.status).toBe(416);
    expect(unsatisfiable.headers.get("content-range")).toBe("bytes */6");
    await unsatisfiable.body?.cancel();

    const head = await remoteFetch("/objects/repo/raw/objects/pack/data.pack", {
      method: "HEAD",
    });
    expect(head.status).toBe(200);
    expect(head.headers.get("content-length")).toBe("6");
  });

  it("streams an upload bundle across transactional storage and R2", async () => {
    await remoteFetch("/upload/repo/raw/refs/heads/main", {
      method: "PUT",
      body: "commit-one\n",
    });
    await remoteFetch("/upload/repo/raw-if-not-exists/objects/pack/one.idx", {
      method: "PUT",
      body: "index",
    });
    await remoteFetch("/upload/repo/raw-if-not-exists/objects/pack/one.pack", {
      method: "PUT",
      body: "pack",
    });

    const response = await remoteFetch("/upload/repo/upload-bundle/refs/heads/main", {
      method: "POST",
    });
    expect(response.status).toBe(200);
    const manifestBytes = Number(response.headers.get("x-graft-bundle-manifest-bytes"));
    expect(manifestBytes).toBeGreaterThan(0);
    const bytes = new Uint8Array(await response.arrayBuffer());
    expect(decodeUploadBundle(bytes, manifestBytes)).toEqual([
      ["objects/pack/one.idx", "index"],
      ["objects/pack/one.pack", "pack"],
    ]);
  });

  it("performs atomic ref compare-and-swap and compare-and-delete", async () => {
    const ref = "/cas/repo/cas/refs/heads/main";
    const created = await remoteFetch(ref, {
      method: "POST",
      headers: {
        "x-graft-expected-present": "false",
        "x-graft-expected-hex": "",
      },
      body: "a\n",
    });
    expect(created.status).toBe(204);

    const contenders = await Promise.all([
      remoteFetch(ref, {
        method: "POST",
        headers: {
          "x-graft-expected-present": "true",
          "x-graft-expected-hex": "610a",
        },
        body: "b\n",
      }),
      remoteFetch(ref, {
        method: "POST",
        headers: {
          "x-graft-expected-present": "true",
          "x-graft-expected-hex": "610a",
        },
        body: "c\n",
      }),
    ]);
    expect(contenders.map((response) => response.status).sort()).toEqual([204, 409]);
    await Promise.all(contenders.map((response) => response.body?.cancel()));

    const current = await remoteFetch("/cas/repo/raw/refs/heads/main");
    const value = await responseText(current);
    expect(["b\n", "c\n"]).toContain(value);

    const expectedHex = value === "b\n" ? "620a" : "630a";
    const staleDelete = await remoteFetch("/cas/repo/cad/refs/heads/main", {
      method: "POST",
      headers: {
        "x-graft-expected-present": "true",
        "x-graft-expected-hex": "610a",
      },
    });
    expect(staleDelete.status).toBe(409);
    await staleDelete.body?.cancel();

    const deleted = await remoteFetch("/cas/repo/cad/refs/heads/main", {
      method: "POST",
      headers: {
        "x-graft-expected-present": "true",
        "x-graft-expected-hex": expectedHex,
      },
    });
    expect(deleted.status).toBe(204);

    const missing = await remoteFetch("/cas/repo/raw/refs/heads/main");
    expect(missing.status).toBe(404);
    await missing.body?.cancel();
  });

  it("lists transactional and immutable paths in lexical order", async () => {
    await remoteFetch("/list/repo/raw-if-not-exists/refs/heads/main", {
      method: "PUT",
      body: "main\n",
    });
    await remoteFetch("/list/repo/raw-if-not-exists/refs/heads/feature/search", {
      method: "PUT",
      body: "feature\n",
    });
    await remoteFetch("/list/repo/raw-if-not-exists/objects/bb/two", {
      method: "PUT",
      body: "two",
    });
    await remoteFetch("/list/repo/raw-if-not-exists/objects/aa/one", {
      method: "PUT",
      body: "one",
    });

    const all = await remoteFetch("/list/repo/list?prefix=");
    expect(await all.json()).toEqual({
      paths: [
        "objects/aa/one",
        "objects/bb/two",
        "refs/heads/feature/search",
        "refs/heads/main",
      ],
    });

    const refs = await remoteFetch("/list/repo/list?prefix=refs%2Fheads%2F");
    expect(await refs.json()).toEqual({
      paths: ["refs/heads/feature/search", "refs/heads/main"],
    });

    for (const query of ["prefix=&prefix=refs", "prefix=&unknown=value", "limit=501"]) {
      const invalid = await remoteFetch(`/list/repo/list?${query}`);
      expect(invalid.status).toBe(400);
      await invalid.body?.cancel();
    }
  });

  it("paginates across SQLite and R2 with a stable global cursor", async () => {
    const entries = [
      ["HEAD", "head"],
      ["alpha/start", "alpha"],
      ["objects/aa/one", "object"],
      ["refs/heads/alpha", "ref-a"],
      ["refs/heads/zeta", "ref-z"],
      ["scratch/end", "scratch"],
    ] as const;
    for (const [path, value] of entries) {
      const response = await remoteFetch(`/pagination/repo/raw-if-not-exists/${path}`, {
        method: "PUT",
        body: value,
      });
      expect(response.status).toBe(204);
    }

    const paths: string[] = [];
    let cursor: string | undefined;
    do {
      const query = new URLSearchParams({ limit: "2" });
      if (cursor === undefined) {
        query.set("prefix", "");
      } else {
        query.set("cursor", cursor);
      }
      const response = await remoteFetch(`/pagination/repo/list?${query}`);
      expect(response.status).toBe(200);
      const page = (await response.json()) as { paths: string[]; next_cursor?: string };
      expect(page.paths.length).toBeLessThanOrEqual(2);
      paths.push(...page.paths);
      cursor = page.next_cursor;
    } while (cursor !== undefined);

    expect(paths).toEqual(entries.map(([path]) => path));
    expect(new Set(paths).size).toBe(paths.length);
  });

  it("lists transactional paths with astral Unicode prefixes", async () => {
    const paths = ["refs/heads/\ue000", "refs/heads/𐀀", "refs/heads/🚀-feature"];
    for (const path of paths) {
      const encodedPath = path.split("/").map(encodeURIComponent).join("/");
      const response = await remoteFetch(`/unicode/repo/raw-if-not-exists/${encodedPath}`, {
        method: "PUT",
        body: path,
      });
      expect(response.status).toBe(204);
    }

    const allQuery = new URLSearchParams({ prefix: "refs/heads/" });
    const all = await remoteFetch(`/unicode/repo/list?${allQuery}`);
    expect(await all.json()).toEqual({ paths });

    const query = new URLSearchParams({ prefix: "refs/heads/🚀" });
    const response = await remoteFetch(`/unicode/repo/list?${query}`);
    expect(await response.json()).toEqual({ paths: ["refs/heads/🚀-feature"] });
  });

  it("isolates repositories and preserves metadata across Durable Object restarts", async () => {
    await remoteFetch("/isolation/one/raw-if-not-exists/HEAD", {
      method: "PUT",
      body: "one",
    });
    await remoteFetch("/isolation/two/raw-if-not-exists/HEAD", {
      method: "PUT",
      body: "two",
    });

    await abortAllDurableObjects();

    expect(await responseText(await remoteFetch("/isolation/one/raw/HEAD"))).toBe("one");
    expect(await responseText(await remoteFetch("/isolation/two/raw/HEAD"))).toBe("two");
  });

  it("accepts the legacy technical route while advertising the canonical repository", async () => {
    const response = await remoteFetch("/api/graft/v1/repos/legacy/repo");
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ repository: "legacy/repo" });
  });

  it("rejects unsafe paths, immutable overwrites and invalid expected values", async () => {
    const overwrite = await remoteFetch("/safety/repo/raw/objects/aa/value", {
      method: "PUT",
      body: "value",
    });
    expect(overwrite.status).toBe(405);
    expect(overwrite.headers.get("allow")).toBe("GET, HEAD");
    await overwrite.body?.cancel();

    const reserved = await remoteFetch("/safety/repo/raw-if-not-exists/locks/ref.lock", {
      method: "PUT",
      body: "value",
    });
    expect(reserved.status).toBe(400);
    await reserved.body?.cancel();

    const invalidExpected = await remoteFetch("/safety/repo/cas/refs/heads/main", {
      method: "POST",
      headers: {
        "x-graft-expected-present": "true",
        "x-graft-expected-hex": "ABC",
      },
      body: "value",
    });
    expect(invalidExpected.status).toBe(400);
    await invalidExpected.body?.cancel();

    const bytes = await remoteFetch("/safety/repo/raw-if-not-exists/objects/binary", {
      method: "PUT",
      body: new Uint8Array([0, 1, 2, 255]),
    });
    expect(bytes.status).toBe(204);
    expect(await responseBytes(await remoteFetch("/safety/repo/raw/objects/binary"))).toEqual([
      0, 1, 2, 255,
    ]);
  });
});
