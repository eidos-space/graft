import { Hono } from "hono";
import { describe, expect, it } from "vitest";

import {
  GraftProtocolError,
  createGraftRemote,
  type GraftRepositoryBackend,
} from "../src/index.js";

const ORIGIN = "https://remote.example";

describe("createGraftRemote", () => {
  it("routes encrypted aggregates through authorization and the shared protocol handler", async () => {
    const key = `e2ee/packs/${"a".repeat(64)}`;
    const operations: string[] = [];
    const backend: GraftRepositoryBackend = {
      encryption: {profile:"encrypted-remote-v1", version:1, suite:1, uuid:"123e4567-e89b-42d3-a456-426614174000"},
      head: () => ({size:1}), get: () => ({size:1, body:new Uint8Array([7])}),
      put: () => undefined, delete: () => undefined, putIfAbsent: () => false,
      compareAndSwap: () => false, compareAndDelete: () => false,
      list: () => ({paths:[], hasMore:false}),
    };
    const app = createGraftRemote({backend: () => backend, authorize: context => {operations.push(context.operation); expect(context.action).toBe(context.operation === "descriptor" ? "discover" : "read");}});
    const body = JSON.stringify({version:1, keys:[key]});
    const response = await app.request(`${ORIGIN}/alice/repo/e2ee/read-bundle`, {method:"POST", body, headers:{"Graft-Protocol":"1", "Content-Length":String(new TextEncoder().encode(body).length)}});
    expect(response.status).toBe(200);
    expect(response.headers.get("x-graft-bundle-objects")).toBe("1");
    const bytes = new Uint8Array(await response.arrayBuffer());
    expect(new TextDecoder().decode(bytes.subarray(0, 8))).toBe("GRAFTEB1");
    expect(operations).toEqual(["e2ee-read-bundle"]);
    const insecure = await app.request("http://remote.example/alice/repo", {headers:{"Graft-Protocol":"1"}});
    expect(insecure.status).toBe(400);
  });
  it("mounts the core handler without taking over unrelated Hono routes", async () => {
    let objectPath: string | undefined;
    let adapterPath: string | undefined;
    const backend: GraftRepositoryBackend = {
      head(path) {
        objectPath = path;
        return { size: 0 };
      },
      get: () => null,
      put: () => undefined,
      delete: () => undefined,
      putIfAbsent: () => false,
      compareAndSwap: () => false,
      compareAndDelete: () => false,
      list: () => ({ paths: [], hasMore: false }),
    };
    const remote = createGraftRemote<{}, string>({
      legacyRoutes: true,
      authenticate({ request, adapterContext }) {
        adapterPath = adapterContext.req.path;
        if (request.headers.get("Authorization") !== "Bearer test-token") {
          throw new GraftProtocolError(401, "unauthorized", "Authentication required");
        }
        return "user-1";
      },
      backend: () => backend,
    });
    const app = new Hono();
    app.get("/health", (context) => context.text("ok"));
    app.route("/graft", remote);

    expect(await (await app.request(`${ORIGIN}/health`)).text()).toBe("ok");
    expect((await app.request(`${ORIGIN}/graft/not-a-repository`)).status).toBe(404);

    const unauthorized = await app.request(`${ORIGIN}/graft/acme/archive`, {
      headers: { "Graft-Protocol": "1" },
    });
    expect(unauthorized.status).toBe(401);

    const headers = {
      Authorization: "Bearer test-token",
      "Graft-Protocol": "1",
    };
    const descriptor = await app.request(`${ORIGIN}/graft/acme/archive`, { headers });
    expect(descriptor.status).toBe(200);
    expect(await descriptor.json()).toMatchObject({ repository: "acme/archive" });
    expect(adapterPath).toBe("/graft/acme/archive");

    const object = await app.request(
      `${ORIGIN}/graft/acme/archive/raw/refs/heads/main`,
      { method: "HEAD", headers },
    );
    expect(object.status).toBe(200);
    expect(objectPath).toBe("refs/heads/main");

    const legacy = await app.request(
      `${ORIGIN}/graft/api/graft/v1/repos/legacy/repo`,
      { headers },
    );
    expect(legacy.status).toBe(200);
    expect(await legacy.json()).toMatchObject({ repository: "legacy/repo" });
  });
});
