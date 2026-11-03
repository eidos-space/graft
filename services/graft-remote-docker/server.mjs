import { createServer } from "node:http";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { createApp, problem, validateOrigin, validateToken } from "./app.mjs";
import { Storage, MAX_BYTES } from "./storage.mjs";

process.umask(0o077);
const origin = validateOrigin(process.env.GRAFT_PUBLIC_ORIGIN ?? "http://127.0.0.1:8787");
// File only: the secret never appears in Docker environment inspection or argv.
const token = validateToken(
  readFileSync(process.env.GRAFT_ACCESS_KEY_FILE ?? "/run/secrets/graft_access_key", "utf8").trim(),
);
const directory = process.env.GRAFT_DATA_DIR ?? "/data";
mkdirSync(directory, { recursive: true, mode: 0o700 });
const storage = new Storage(join(directory, "graft.sqlite"));
const app = createApp({ storage, origin, token });
let active = 0;
let closing = false;
const server = createServer(
  { requestTimeout: 120_000, headersTimeout: 15_000, maxHeaderSize: 16_384 },
  async (incoming, outgoing) => {
    let counted = false;
    try {
      let response;
      if (closing || active >= 4)
        response = problem(503, "busy", "Retry later", { "Retry-After": "1" });
      else if (Number(incoming.headers["content-length"] ?? 0) > MAX_BYTES)
        response = problem(413, "request_too_large", "Request exceeds 64 MiB");
      else if (!incoming.url?.startsWith("/") || incoming.url.startsWith("//"))
        response = problem(400, "invalid_path", "Use an origin-relative path");
      else {
        active += 1;
        counted = true;
        const headers = new Headers();
        for (let index = 0; index < incoming.rawHeaders.length; index += 2)
          headers.append(incoming.rawHeaders[index], incoming.rawHeaders[index + 1]);
        const cancellation = new AbortController();
        incoming.once("aborted", () => cancellation.abort());
        outgoing.once("close", () => {
          if (!outgoing.writableFinished) cancellation.abort();
        });
        const body = ["GET", "HEAD"].includes(incoming.method)
          ? undefined
          : Readable.toWeb(incoming);
        // Canonical configured origin, never client-supplied Host/Forwarded headers.
        response = await app(
          new Request(`${origin}${incoming.url}`, {
            method: incoming.method,
            headers,
            body,
            duplex: "half",
            signal: cancellation.signal,
          }),
        );
      }
      if (!incoming.complete) response.headers.set("Connection", "close");
      outgoing.writeHead(response.status, Object.fromEntries(response.headers));
      if (response.body && incoming.method !== "HEAD")
        await pipeline(Readable.fromWeb(response.body), outgoing);
      else outgoing.end();
    } catch {
      if (!outgoing.headersSent)
        outgoing.writeHead(500, { "Content-Type": "application/problem+json" });
      outgoing.end();
    } finally {
      if (counted) active -= 1;
    }
  },
);
server.timeout = 120_000;
server.on("timeout", (socket) => socket.destroy());
server.listen(8787, "0.0.0.0", () => console.log("Graft server listening on port 8787"));
function shutdown() {
  if (closing) return;
  closing = true;
  const deadline = setTimeout(() => {
    server.closeAllConnections();
  }, 25_000);
  deadline.unref();
  server.close(() => {
    clearTimeout(deadline);
    storage.close();
  });
}
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
