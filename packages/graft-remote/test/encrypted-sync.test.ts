import { describe, expect, it } from "vitest";
import {
  createGraftRemoteHandler,
  bytesEqual,
  type GraftRepositoryBackend,
  type GraftWriteBody,
  type GraftObject,
} from "../src/index.js";

const text = new TextEncoder();
const uuid = "00010203-0405-4607-8809-0a0b0c0d0e0f";
const identity = { profile: "encrypted-remote-v1", version: 1, suite: 1, uuid } as const;
const token = (digit: string) => digit.repeat(64);
const key = (kind: string, digit: string) => `e2ee/${kind}/${token(digit)}`;
const referenceKey = key("refs", "a");
function hex(value: string): Uint8Array<ArrayBuffer> {
  return Uint8Array.from(value.match(/../g)!, (pair) => Number.parseInt(pair, 16));
}
function number(value: number, width: 2 | 4 | 8): Uint8Array<ArrayBuffer> {
  const bytes = new Uint8Array(width);
  const view = new DataView(bytes.buffer);
  if (width === 2) view.setUint16(0, value);
  else if (width === 4) view.setUint32(0, value);
  else view.setBigUint64(0, BigInt(value));
  return bytes;
}
function concat(...parts: Uint8Array<ArrayBuffer>[]): Uint8Array<ArrayBuffer> {
  const bytes = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    bytes.set(part, offset);
    offset += part.length;
  }
  return bytes;
}
function fields(...parts: Uint8Array<ArrayBuffer>[]): Uint8Array<ArrayBuffer> {
  return concat(
    number(parts.length, 2),
    ...parts.flatMap((part) => [number(part.length, 4), part]),
  );
}
function ref(digit: string): Uint8Array<ArrayBuffer> {
  return concat(
    text.encode("GRAFTER1"),
    number(1, 2),
    number(1, 2),
    Uint8Array.of(1),
    hex(token(digit)),
    new Uint8Array(40),
  );
}
function object(): Uint8Array<ArrayBuffer> {
  const value = new Uint8Array(104);
  value.set(text.encode("GRAFTEO1"));
  value[9] = 1;
  value[11] = 1;
  new DataView(value.buffer).setUint32(68, 16);
  return value;
}
function hint(pack: string, commits: [string, string[]][]): Uint8Array<ArrayBuffer> {
  return concat(
    text.encode("GRAFTEH1"),
    number(1, 2),
    number(1, 2),
    hex(uuid.replaceAll("-", "")),
    fields(
      text.encode(key("packs", pack)),
      text.encode(key("indexes", pack)),
      fields(
        ...commits
          .sort(([a], [b]) => a.localeCompare(b))
          .map(([commit, parents]) =>
            fields(hex(token(commit)), fields(...parents.map((parent) => hex(token(parent))))),
          ),
      ),
    ),
    new Uint8Array(32),
  );
}
function bundle(
  entries: [string, Uint8Array<ArrayBuffer>][],
  replacement = ref("1"),
): Uint8Array<ArrayBuffer> {
  const sorted = [...entries].sort(([left], [right]) => left.localeCompare(right));
  const manifest = fields(
    number(1, 2),
    text.encode(referenceKey),
    replacement,
    fields(...sorted.map(([name, bytes]) => fields(text.encode(name), number(bytes.length, 8)))),
  );
  return concat(
    text.encode("GRAFTEB1"),
    number(manifest.length, 4),
    manifest,
    ...sorted.map(([name, bytes]) =>
      concat(
        number(text.encode(name).length, 4),
        number(bytes.length, 8),
        text.encode(name),
        bytes,
      ),
    ),
  );
}
async function bodyBytes(body: GraftWriteBody): Promise<Uint8Array<ArrayBuffer>> {
  return body instanceof Uint8Array
    ? body.slice()
    : new Uint8Array(await new Response(body).arrayBuffer());
}
function fixture() {
  const objects = new Map<string, Uint8Array<ArrayBuffer>>();
  let writes = 0,
    compares = 0,
    lists = 0;
  let onHead: (() => void) | undefined;
  const backend: GraftRepositoryBackend = {
    encryption: identity,
    head(path) {
      onHead?.();
      const bytes = objects.get(path);
      return bytes === undefined ? null : { size: bytes.length };
    },
    get(path, range): GraftObject | null {
      const bytes = objects.get(path);
      if (bytes === undefined) return null;
      return {
        size: bytes.length,
        body: range === undefined ? bytes.slice() : bytes.slice(range.start, range.end + 1),
      };
    },
    put(path, bytes) {
      objects.set(path, bytes.slice());
      writes += 1;
    },
    delete(path) {
      objects.delete(path);
    },
    async putIfAbsent(path, value) {
      if (objects.has(path)) return false;
      const bytes = await bodyBytes(value);
      if (objects.has(path)) return false;
      objects.set(path, bytes);
      writes += 1;
      return true;
    },
    compareAndSwap(path, expected, replacement) {
      compares += 1;
      const current = objects.get(path);
      if (
        current === undefined
          ? expected !== undefined
          : expected === undefined || !bytesEqual(current, expected)
      )
        return false;
      objects.set(path, replacement.slice());
      return true;
    },
    compareAndDelete(path, expected) {
      const current = objects.get(path);
      if (
        current === undefined
          ? expected !== undefined
          : expected === undefined || !bytesEqual(current, expected)
      )
        return false;
      objects.delete(path);
      return true;
    },
    list({ prefix, after, limit }) {
      lists += 1;
      const paths = [...objects.keys()]
        .filter((value) => value.startsWith(prefix) && (after === undefined || value > after))
        .sort();
      return { paths: paths.slice(0, limit), hasMore: paths.length > limit };
    },
  };
  let handler = createGraftRemoteHandler({ backend: () => backend });
  return {
    objects,
    backend,
    counts: () => ({ writes, compares, lists }),
    setHeadHook: (callback: () => void) => {
      onHead = callback;
    },
    restart: () => {
      handler = createGraftRemoteHandler({ backend: () => backend });
    },
    async call(action: string, body: Uint8Array<ArrayBuffer>, expected?: Uint8Array<ArrayBuffer>) {
      const receive = action === "receive-bundle";
      const suffix = `${action}/${referenceKey}`;
      const headers = new Headers({
        "Graft-Protocol": "1",
        "content-length": String(body.length),
        "content-type": receive ? "application/vnd.graft.e2ee-bundle" : "application/json",
      });
      if (receive) {
        headers.set("x-graft-expected-present", String(expected !== undefined));
        headers.set(
          "x-graft-expected-hex",
          expected === undefined
            ? ""
            : [...expected].map((b) => b.toString(16).padStart(2, "0")).join(""),
        );
      }
      return handler({
        request: new Request(`https://remote.example/acme/encrypted/e2ee/${suffix}`, {
          method: "POST",
          headers,
          body,
        }),
        route: {
          namespace: "acme",
          repository: "encrypted",
          operation: "e2ee",
          objectPath: suffix,
        },
        adapterContext: undefined,
      });
    },
  };
}
function entries(pack: string, commits: [string, string[]][]): [string, Uint8Array<ArrayBuffer>][] {
  return [
    [key("packs", pack), object()],
    [key("indexes", pack), object()],
    [key("hints", pack), hint(pack, commits)],
  ];
}
function fetchBody(have: string | null = null): Uint8Array<ArrayBuffer> {
  return text.encode(JSON.stringify({ version: 1, have }));
}

describe("encrypted aggregate synchronization", () => {
  it("publishes complete immutable frames before CAS and reuses identical ciphertext", async () => {
    const app = fixture();
    const payload = bundle(entries("b", [["1", []]]));
    expect((await app.call("receive-bundle", payload)).status).toBe(204);
    expect(app.counts()).toMatchObject({ writes: 3, compares: 1 });
    expect((await app.call("receive-bundle", payload, ref("1"))).status).toBe(204);
    expect(app.counts()).toMatchObject({ writes: 3, compares: 2 });
    expect(app.objects.get(referenceKey)).toEqual(ref("1"));
  });

  it("never advances ref on immutable collisions, malformed or partial receive", async () => {
    for (const alteration of ["collision", "truncated", "trailing", "duplicate", "mutable"]) {
      const app = fixture();
      let frames = entries("b", [["1", []]]);
      if (alteration === "collision") {
        const corrupt = object();
        corrupt[103] = corrupt[103]! ^ 1;
        app.objects.set(key("packs", "b"), corrupt);
      }
      if (alteration === "duplicate") frames = [...frames, frames[0]!];
      if (alteration === "mutable") frames = [...frames, [referenceKey, ref("1")]];
      let payload = bundle(frames);
      if (alteration === "truncated") payload = payload.slice(0, -1);
      if (alteration === "trailing") payload = concat(payload, Uint8Array.of(1));
      expect((await app.call("receive-bundle", payload)).status).toBe(
        alteration === "collision" ? 412 : 400,
      );
      expect(app.counts().compares).toBe(0);
      expect(app.objects.has(referenceKey)).toBe(false);
    }
  });

  it("returns a ref conflict only after complete immutable publication", async () => {
    const app = fixture();
    app.objects.set(referenceKey, ref("2"));
    expect((await app.call("receive-bundle", bundle(entries("b", [["1", []]])))).status).toBe(409);
    expect(app.counts()).toMatchObject({ writes: 3, compares: 1 });
    expect(app.objects.get(referenceKey)).toEqual(ref("2"));
  });

  it("uses incrementally published hints and rebuilds after cache loss", async () => {
    const app = fixture();
    expect((await app.call("receive-bundle", bundle(entries("b", [["1", []]])))).status).toBe(204);
    const first = await app.call("fetch-bundle", fetchBody());
    expect(first.status).toBe(200);
    expect(first.headers.get("x-graft-bundle-objects")).toBe("3");
    expect(new Uint8Array(await first.arrayBuffer())).toEqual(bundle(entries("b", [["1", []]])));
    expect(app.counts().lists).toBe(0);
    app.restart();
    const cold = await app.call("fetch-bundle", fetchBody());
    expect(cold.status).toBe(200);
    await cold.arrayBuffer();
    expect(app.counts().lists).toBe(1);
    const same = await app.call("fetch-bundle", fetchBody(token("1")));
    expect(same.status).toBe(200);
    expect(same.headers.get("x-graft-bundle-objects")).toBe("0");
    await same.arrayBuffer();
    expect(app.counts().lists).toBe(1);
  });

  it("stops graph traversal at have while retaining exact ref bytes", async () => {
    const app = fixture();
    for (const entry of [...entries("b", [["1", []]]), ...entries("c", [["2", ["1"]]])])
      app.objects.set(...entry);
    app.objects.set(referenceKey, ref("2"));
    const response = await app.call("fetch-bundle", fetchBody(token("1")));
    expect(response.status).toBe(200);
    expect(new Uint8Array(await response.arrayBuffer())).toEqual(
      bundle(entries("c", [["2", ["1"]]]), ref("2")),
    );
  });

  it("rejects missing coverage, cycles and conflicting parent hints", async () => {
    for (const problem of ["missing", "cycle", "conflict"]) {
      const app = fixture();
      app.objects.set(referenceKey, ref("1"));
      const topology: [string, string[]][] =
        problem === "cycle"
          ? [
              ["1", ["2"]],
              ["2", ["1"]],
            ]
          : [["1", ["2"]]];
      for (const entry of entries("b", topology)) app.objects.set(...entry);
      if (problem === "conflict")
        for (const entry of entries("c", [["1", []]])) app.objects.set(...entry);
      expect((await app.call("fetch-bundle", fetchBody())).status).toBe(422);
    }
  });

  it("detects a ref that changes during bundle planning", async () => {
    const app = fixture();
    for (const entry of entries("b", [["1", []]])) app.objects.set(...entry);
    app.objects.set(referenceKey, ref("1"));
    app.setHeadHook(() => {
      app.objects.set(referenceKey, ref("2"));
    });
    expect((await app.call("fetch-bundle", fetchBody())).status).toBe(409);
  });

  it("rejects duplicate/unknown control fields and noncanonical route tokens", async () => {
    const app = fixture();
    for (const body of [
      '{"version":1,"have":null,"have":null}',
      '{"version":1,"have":null,"extra":0}',
      '{"version":1,"have":"ABC"}',
      '{"version":1}',
    ]) {
      expect((await app.call("fetch-bundle", text.encode(body))).status).toBe(400);
    }
    expect(app.counts()).toEqual({ writes: 0, compares: 0, lists: 0 });
  });
});
