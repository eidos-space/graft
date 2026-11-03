import { describe, expect, it } from "vitest";
import { createGraftRemoteHandler, GraftProtocolError, type GraftRepositoryBackend } from "../src/index.js";

const uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
const key = `e2ee/files/${"a".repeat(64)}`;
const challenge = Uint8Array.from({ length: 32 }, (_, index) => index);
const expectedPrefix = "47524146542d453245452d4f424a4543542d50524f4f462d563100000102030405460788090a0b0c0d0e0f0000004b653265652f66696c65732f616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161616161610000000000000005000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const expectedDigest = "4f904ed57dc3ecc8393ec4494eaa74bc6682a63a067043e3a689490d45e0532a";
const hex = (bytes: Uint8Array) => Array.from(bytes, b => b.toString(16).padStart(2, "0")).join("");

function fixture() {
  let stored: Uint8Array<ArrayBuffer> | null = Uint8Array.of(0, 1, 2, 254, 255);
  let calls = 0;
  let prefixHex = "";
  const backend: GraftRepositoryBackend = {
    encryption: { profile: "encrypted-remote-v1", version: 1, suite: 1, uuid },
    head: () => null, get: () => null, put: () => {}, delete: () => {},
    putIfAbsent: () => false, compareAndSwap: () => false, compareAndDelete: () => false,
    list: () => ({ paths: [], hasMore: false }),
    async encryptedObjectProof(path, prefix) {
      calls++;
      expect(path).toBe(key);
      prefixHex = hex(prefix);
      if (stored === null) return null;
      const input = new Uint8Array(prefix.length + stored.length);
      input.set(prefix); input.set(stored, prefix.length);
      return { size: stored.length, sha256: hex(new Uint8Array(await crypto.subtle.digest("SHA-256", input))) };
    },
  };
  let deny = false;
  const handler = createGraftRemoteHandler({
    backend: () => backend,
    authorize: ({ action }) => {
      expect(action === "read" || action === "discover").toBe(true);
      if (deny) throw new GraftProtocolError(403, "forbidden", "Denied");
    },
  });
  return { backend, setStored: (value: typeof stored) => { stored = value; }, deny: () => { deny = true; },
    calls: () => calls, prefix: () => prefixHex,
    async call(options: { path?: string; method?: string; headers?: Record<string, string>; body?: Uint8Array<ArrayBuffer>; descriptor?: boolean; urlPath?: string } = {}) {
      const path = options.path ?? key;
      const suffix = options.descriptor ? "" : `e2ee/object-proof/${path}`;
      const method = options.method ?? (options.descriptor ? "GET" : "POST");
      return handler({ request: new Request(`https://remote.example/acme/repo/${options.urlPath ?? suffix}`, {
        method, headers: { "Graft-Protocol": "1", "Content-Type": "application/octet-stream", "Content-Length": "32", "X-Graft-Object-Bytes": "5", ...options.headers },
        ...(method === "POST" ? { body: options.body ?? challenge } : {}),
      }), route: { namespace: "acme", repository: "repo", ...(options.descriptor ? {} : { operation: "e2ee", objectPath: `object-proof/${path}` }) }, adapterContext: undefined });
    },
  };
}

describe("encrypted stored-object integrity challenge", () => {
  it("matches the fixed cross-language byte vector and hashes actual bytes for each challenge", async () => {
    const f = fixture();
    const response = await f.call();
    expect(response.status).toBe(200);
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(await response.json()).toEqual({ version: 1, uuid, key, ciphertext_bytes: 5, challenge: hex(challenge), sha256: expectedDigest });
    expect(f.prefix()).toBe(expectedPrefix);
    expect((await (await f.call({ body: new Uint8Array(32) })).json()).sha256).not.toBe(expectedDigest);
    f.setStored(Uint8Array.of(0, 1, 2, 254, 0));
    expect((await (await f.call()).json()).sha256).not.toBe(expectedDigest);
    expect(f.calls()).toBe(3);
  });
  it("advertises only an available encrypted callback and authorizes before reading", async () => {
    const f = fixture();
    expect((await (await f.call({ descriptor: true })).json()).capabilities).toContain("e2ee-object-proof-v1");
    delete f.backend.encryptedObjectProof;
    expect((await (await f.call({ descriptor: true })).json()).capabilities).not.toContain("e2ee-object-proof-v1");
    expect((await f.call()).status).toBe(501);
    f.deny();
    expect((await f.call()).status).toBe(403);
    expect(f.calls()).toBe(0);
  });
  it("rejects missing, truncated and invalid backend results", async () => {
    const f = fixture();
    f.setStored(null); expect((await f.call()).status).toBe(404);
    f.setStored(Uint8Array.of(1)); expect((await f.call()).status).toBe(409);
    f.backend.encryptedObjectProof = () => ({ size: 5, sha256: "not-a-digest" });
    expect((await f.call()).status).toBe(500);
  });
  it("rejects malformed paths, methods, headers and bodies before backend work", async () => {
    const f = fixture();
    const cases: Parameters<typeof f.call>[0][] = [
      { path: "e2ee/config" }, { path: `e2ee/refs/${"a".repeat(64)}` },
      { headers: { "X-Graft-Object-Bytes": "05" } }, { headers: { "Content-Length": "31" } },
      { headers: { "Content-Type": "application/json" } }, { body: new Uint8Array(31) },
      { urlPath: `e2ee/object-proof/e2ee/files/%61${"a".repeat(63)}` },
      { urlPath: `e2ee/object-proof/e2ee%2ffiles/${"a".repeat(64)}` },
    ];
    for (const options of cases) expect((await f.call(options)).status).toBe(400);
    expect((await f.call({ method: "GET" })).status).toBe(405);
    expect((await f.call({ body: new Uint8Array(33) })).status).toBe(413);
    expect(f.calls()).toBe(0);
  });
});
