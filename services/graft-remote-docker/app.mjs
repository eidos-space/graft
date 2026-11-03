import { createHash, timingSafeEqual } from "node:crypto";
import {
  createGraftRemoteHandler,
  GraftProtocolError,
} from "../../packages/graft-remote/dist/index.js";
import { descriptor, MAX_BYTES, readBytes } from "./storage.mjs";

export function validateOrigin(value) {
  const url = new URL(value);
  if (
    url.username ||
    url.password ||
    url.search ||
    url.hash ||
    url.pathname !== "/" ||
    (url.protocol !== "https:" &&
      !(url.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)))
  ) {
    throw new Error("GRAFT_PUBLIC_ORIGIN must be an HTTPS origin (HTTP only on loopback)");
  }
  return url.origin;
}

export function validateToken(token) {
  if (
    token.length < 32 ||
    token.length > 512 ||
    /\s/.test(token) ||
    new Set(token).size < 8 ||
    /^(replace|change[-_]?me|example|your[-_])/i.test(token)
  ) {
    throw new Error("Provide a random access key of 32–512 characters");
  }
  return token;
}

export function createApp({ storage, origin, token }) {
  origin = validateOrigin(origin);
  const hash = (value) => createHash("sha256").update(value).digest();
  const expected = hash(validateToken(token));
  function authenticate(request) {
    const header = request.headers.get("authorization") ?? "";
    if (!header.startsWith("Bearer ") || !timingSafeEqual(expected, hash(header.slice(7)))) {
      throw new GraftProtocolError(401, "unauthorized", "A valid bearer access key is required", {
        "WWW-Authenticate": 'Bearer realm="graft-remote"',
      });
    }
  }
  const remote = createGraftRemoteHandler({
    authenticate: ({ request }) => authenticate(request),
    backend({ adapterContext, request }) {
      return storage.backend(adapterContext.row, { signal: request.signal });
    },
    limits: { maxRequestBytes: MAX_BYTES },
  });
  const representation = (row) => ({
    name: row.name,
    status: row.status,
    display_name: row.display_name,
    created_at: row.created_at,
    encryption: descriptor(row.uuid),
    remote_url: `${origin}/personal/${row.name}`,
  });
  return async (request) => {
    try {
      const path = new URL(request.url).pathname;
      if (request.method === "GET" && path === "/healthz") {
        storage.health();
        return json({ service: "graft-docker", status: "ok" });
      }
      if (request.method === "GET" && path === "/")
        return json({
          service: "graft-docker",
          mode: "personal",
          namespace: "personal",
          discovery: "/.well-known/graft",
          health: "/healthz",
        });
      if (request.method === "GET" && path === "/.well-known/graft")
        return json({
          service: "graft-docker",
          protocol: "graft-remote",
          version: 1,
          remote_url_template: `${origin}/personal/{repository}`,
          authentication: { scheme: "bearer", authority: origin },
          management: {
            version: 1,
            capabilities: ["rename", "delete"],
            repositories: `${origin}/api/graft/repositories`,
          },
        });
      authenticate(request);
      if (path === "/api/graft/repositories" && request.method === "GET") {
        return json({ namespace: "personal", repositories: storage.list().map(representation) });
      }
      const statusRoute = /^\/api\/graft\/repositories\/([^/]+)\/status$/.exec(path);
      if (statusRoute && request.method === "GET") {
        const name = decodeURIComponent(statusRoute[1]);
        const row = storage.lookup(name);
        if (!row) throw new GraftProtocolError(404, "repository_not_found", "Repository not found");
        return json(
          { namespace: "personal", repository: name, status: row.status },
          row.status === "deleting" ? 202 : 200,
        );
      }
      const management = /^\/api\/graft\/repositories\/([^/]+)$/.exec(path);
      if (management && request.method === "DELETE") {
        const name = decodeURIComponent(management[1]);
        const row = storage.requestDelete(name);
        if (!row) throw new GraftProtocolError(404, "repository_not_found", "Repository not found");
        return json(
          { namespace: "personal", repository: name, status: row.status },
          row.status === "deleting" ? 202 : 200,
        );
      }
      if (management && ["PUT", "PATCH"].includes(request.method)) {
        const name = decodeURIComponent(management[1]);
        if (!/^[A-Za-z0-9](?:[A-Za-z0-9._-]{0,62}[A-Za-z0-9])?$/.test(name))
          throw new GraftProtocolError(400, "invalid_repository", "Invalid repository name");
        const rename = request.method === "PATCH";
        const payload = await managementPayload(request, rename);
        const displayName = displayNameValue(
          payload.display_name === undefined ? (rename ? undefined : name) : payload.display_name,
        );
        let row, created;
        if (rename) row = storage.rename(name, displayName);
        else {
          const profile =
            payload.encryption === undefined ? "encrypted-remote-v1" : payload.encryption;
          if (!["plain", "encrypted-remote-v1"].includes(profile))
            throw new GraftProtocolError(
              400,
              "invalid_encryption_profile",
              "Unsupported encryption profile",
            );
          ({ row, created } = storage.provision(name, displayName, profile !== "plain"));
        }
        if (!row) throw new GraftProtocolError(404, "repository_not_found", "Repository not found");
        return json(
          {
            ...(rename ? {} : { created }),
            namespace: "personal",
            repository: name,
            ...representation(row),
          },
          created ? 201 : 200,
        );
      }
      const route = /^\/([^/]+)\/([^/]+)(?:\/([^/]+)(?:\/(.+))?)?$/.exec(path);
      if (!route) throw new GraftProtocolError(404, "not_found", "Not found");
      if (decodeURIComponent(route[1]) !== "personal")
        throw new GraftProtocolError(404, "repository_not_found", "Repository not found");
      const repository = decodeURIComponent(route[2]);
      if (route[3] === "e2ee" && route[4]?.startsWith("object-proof/")) {
        const row = storage.lookup(repository);
        if (row && row.status !== "active")
          throw new GraftProtocolError(404, "object_not_found", "Object not found");
      }
      const admission = storage.admit(repository);
      try {
        const response = await remote({
          request,
          route: {
            namespace: decodeURIComponent(route[1]),
            repository: decodeURIComponent(route[2]),
            operation: route[3],
            objectPath: route[4] === undefined ? undefined : decodeURIComponent(route[4]),
          },
          adapterContext: admission,
        });
        return retainAdmission(response, admission.release);
      } catch (error) {
        admission.release();
        throw error;
      }
    } catch (error) {
      if (error instanceof URIError) return problem(400, "invalid_path", "Invalid encoded path");
      if (error instanceof GraftProtocolError)
        return problem(error.status, error.code, error.message, error.headers);
      console.error("Graft server operation failed");
      return problem(500, "internal_error", "Internal server error");
    }
  };
}

function displayNameValue(value) {
  if (typeof value !== "string")
    throw new GraftProtocolError(400, "invalid_display_name", "Display name required");
  const normalized = value.trim().normalize("NFC");
  if ([...normalized].length < 1 || [...normalized].length > 80 || /\p{Cc}/u.test(normalized))
    throw new GraftProtocolError(
      400,
      "invalid_display_name",
      "Display name must contain 1–80 characters without controls",
    );
  return normalized;
}

async function managementPayload(request, rename) {
  const bytes = request.body ? await readBytes(request.body, 4096) : Buffer.alloc(0);
  if (!bytes.length) return {};
  if (
    request.headers.get("content-type")?.split(";")[0].trim().toLowerCase() !== "application/json"
  )
    throw new GraftProtocolError(415, "unsupported_media_type", "Use application/json");
  let value;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch {
    throw new GraftProtocolError(400, "invalid_json", "Invalid JSON");
  }
  const allowed = rename ? ["display_name"] : ["display_name", "encryption"];
  if (
    value === null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    Object.keys(value).some((key) => !allowed.includes(key))
  )
    throw new GraftProtocolError(400, "invalid_payload", "Unexpected management payload");
  return value;
}

export function json(value, status = 200, headers = {}) {
  return Response.json(value, {
    status,
    headers: {
      "Cache-Control": "no-store",
      "Graft-Protocol": "1",
      "X-Content-Type-Options": "nosniff",
      ...headers,
    },
  });
}
export function problem(status, code, message, headers = {}) {
  return json({ error: { code, message } }, status, {
    "Content-Type": "application/problem+json",
    ...headers,
  });
}

// Keep admission through response streaming/cancellation, not only handler return.
function retainAdmission(response, release) {
  if (!response.body) {
    release();
    return response;
  }
  const reader = response.body.getReader();
  const body = new ReadableStream({
    async pull(controller) {
      try {
        const next = await reader.read();
        if (next.done) {
          release();
          controller.close();
        } else controller.enqueue(next.value);
      } catch (error) {
        release();
        controller.error(error);
      }
    },
    async cancel(reason) {
      try {
        await reader.cancel(reason);
      } finally {
        release();
      }
    },
  });
  return new Response(body, { status: response.status, headers: response.headers });
}
