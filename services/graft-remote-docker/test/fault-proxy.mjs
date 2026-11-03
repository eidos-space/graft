// Test-only HTTP transport: never records credentials or object contents.
import { createServer, request } from "node:http";
import { createHash } from "node:crypto";
import { once } from "node:events";

export const payloadPath = (path) =>
  /\/e2ee\/(files|objects|packs|commits|segments|indexes|hints)\//.test(path);
export const payloadRead = (row) =>
  [200, 206].includes(row.status) &&
  !row.headerProbe &&
  ((row.method === "GET" && payloadPath(row.path)) ||
    /\/e2ee\/(read|fetch)-bundle$/.test(row.path));

// Only exact authenticated-header ranges qualify; small payload reads never do.
export function headerProbe(row, body, contentRange, headers) {
  if (row.method !== "GET" || row.status !== 206 || !payloadPath(row.path)) return false;
  const match = /^bytes (\d+)-(\d+)\/(\d+)$/.exec(contentRange ?? "");
  if (!match) return false;
  const [, start, end, total] = match.map(Number);
  if (row.range !== `bytes=${start}-${end}` || body.length !== end - start + 1 || end >= total)
    return false;
  if (start === 0 && end === 71 && body.length === 72) {
    const size = body.readUInt32BE(68);
    if (size < 16 || size > 16384 || 72 + size > total) return false;
    headers.set(row.path, { size, total });
    return true;
  }
  const known = headers.get(row.path);
  return Boolean(known && start === 72 && end === 71 + known.size && total === known.total);
}

export async function faultProxy(origin, { fallback = false, restart } = {}) {
  const records = [];
  const headerRanges = new Map();
  let sequence = 0;
  let fault = null;
  let injected = 0;
  let headPublished = false;
  let interruptedPart;
  let completeLost = false;
  let crashClient;
  let recovery = Promise.resolve();
  const server = createServer((incoming, outgoing) => {
    const row = {
      method: incoming.method,
      path: incoming.url,
      range: incoming.headers.range,
      partNumber: incoming.headers["x-graft-part-number"],
      begin: sequence++,
      uploadBytes: 0,
      downloadBytes: 0,
    };
    records.push(row);
    const payload = payloadPath(row.path) && row.method === "PUT";
    const multipartPart = row.method === "PUT" && row.path.includes("/multipart-part/");
    const multipartComplete = row.method === "POST" && row.path.includes("/multipart-complete/");
    const refWrite = /\/e2ee\/refs\//.test(row.path) && ["PUT", "POST"].includes(row.method);
    const headWrite = /\/e2ee\/head\//.test(row.path) && ["PUT", "POST"].includes(row.method);
    const descriptor = /^\/personal\/[^/]+$/.test(row.path) && row.method === "GET";
    const hash = createHash("sha256");
    if (fault === "client-crash" && headPublished && descriptor) {
      injected++;
      crashClient.kill("SIGKILL");
      outgoing.destroy();
      incoming.resume();
      return;
    }
    const upstream = request(
      new URL(row.path, origin),
      {
        method: incoming.method,
        headers: { ...incoming.headers, host: new URL(origin).host, connection: "close" },
      },
      async (response) => {
        try {
          const chunks = [];
          for await (const chunk of response) chunks.push(chunk);
          let body = Buffer.concat(chunks);
          row.status = response.statusCode;
          row.responseContentType = response.headers["content-type"];
          row.responseProtocol = response.headers["graft-protocol"];
          row.responseContentRange = response.headers["content-range"];
          row.receivedBytes = body.length;
          row.responseHash = createHash("sha256").update(body).digest("hex");
          // Proof JSON contains protocol fields only; never record payload bodies or auth.
          if (row.path.includes("/e2ee/object-proof/") && body.length <= 4096)
            row.proofResponse = body.toString("utf8");
          row.end = sequence++;
          row.headerProbe = headerProbe(row, body, response.headers["content-range"], headerRanges);
          row.uploadHash = hash.digest("hex");
          if (headWrite && row.status === 204) headPublished = true;
          const headers = { ...response.headers, connection: "close" };
          delete headers["transfer-encoding"];
          if (fallback && descriptor && row.status === 200) {
            const value = JSON.parse(body);
            value.capabilities = value.capabilities.filter(
              (cap) => !["e2ee-fetch-bundle", "e2ee-receive-bundle"].includes(cap),
            );
            body = Buffer.from(JSON.stringify(value));
          }
          const lose =
            injected === 0 &&
            row.status === 204 &&
            ((fault === "lost-object-response" && payload) ||
              (fault === "lost-ref-response" && refWrite) ||
              (fault === "lost-multipart-complete" && multipartComplete));
          if (lose) {
            injected++;
            if (multipartComplete) completeLost = true;
            row.responseLost = true;
            outgoing.destroy();
            return;
          }
          if (fault === "server-crash" && headWrite && row.status === 204 && injected === 0) {
            injected++;
            row.responseLost = true;
            outgoing.destroy();
            recovery = restart();
            // Observe rejections immediately; the test awaits the same promise.
            recovery.catch(() => {});
            return;
          }
          if (fault === "lost-multipart-complete" && completeLost && row.path.includes("/e2ee/object-proof/")) {
            row.responseLost = true;
            outgoing.destroy();
            return;
          }
          row.downloadBytes = body.length;
          headers["content-length"] = String(body.length);
          outgoing.writeHead(row.status, headers);
          outgoing.end(body);
        } catch {
          outgoing.destroy();
        }
      },
    );
    upstream.on("error", () => outgoing.destroy());
    let truncated = false;
    incoming.on("data", (chunk) => {
      if (truncated) return;
      row.uploadBytes += chunk.length;
      hash.update(chunk);
      if (fault === "interrupt-multipart-part" && multipartPart && !interruptedPart)
        interruptedPart = `${row.path}:${row.partNumber}`;
      if ((fault === "interrupt-upload" && payload) ||
          (fault === "interrupt-multipart-part" && multipartPart && `${row.path}:${row.partNumber}` === interruptedPart)) {
        // Send a prefix with the original Content-Length, then break the stream.
        // Keep the fault armed across native retries until the caller reopens.
        truncated = true;
        injected++;
        row.truncated = true;
        upstream.write(
          chunk.subarray(
            0,
            Math.min(32, chunk.length, Math.max(0, Number(incoming.headers["content-length"]) - 1)),
          ),
          () => {
            upstream.destroy();
            outgoing.destroy();
          },
        );
        incoming.resume();
      } else if (!upstream.write(chunk)) incoming.pause();
    });
    upstream.on("drain", () => incoming.resume());
    incoming.on("end", () => {
      if (!truncated) upstream.end();
    });
    incoming.on("error", () => upstream.destroy());
    outgoing.on("close", () => {
      if (!outgoing.writableFinished) upstream.destroy();
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  return {
    origin: `http://127.0.0.1:${server.address().port}`,
    records,
    arm(value, child) {
      fault = value;
      crashClient = child;
    },
    clear() {
      fault = null;
      headerRanges.clear();
    },
    get injected() {
      return injected;
    },
    get recovered() {
      return recovery;
    },
    async close() {
      server.closeAllConnections();
      await new Promise((resolve) => server.close(resolve));
    },
  };
}
