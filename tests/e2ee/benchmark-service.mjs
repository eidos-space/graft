// Synthetic loopback benchmark service. Metrics stay in memory during timed I/O.
import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { Readable, Transform } from "node:stream";
import { createInterface } from "node:readline";
import { createGraftRemoteHandler } from "../../packages/graft-remote/dist/index.js";

const fixture = JSON.parse(readFileSync(process.argv[2], "utf8"));
const emptyMetrics = () => ({ requests: 0, request_body_bytes: 0, response_body_bytes: 0, operation_counts: Object.create(null) });
let metrics = emptyMetrics();
// Opt-in CLI fault verification only; ordinary benchmark fixtures keep their
// existing handler and metrics shape. No production fault route is introduced.
const cliFault = fixture.cliDropConfigAcknowledgements === true
  ? { armed: false, cas_bodies: [], dropped_ack: false, dropped_read: false, dropped_dispatch: false, held_commit: false, held_dispatch: false } : null;
let dropConfigRead = false;
let dropNextConfigDispatch = false;
let holdNextConfigCommit = false;
let holdNextConfigDispatch = false;
function backend(encryption) {
  const objects = new Map(encryption ? [["e2ee/config", Uint8Array.from(fixture.config)]] : []);
  const hints = new Map();
  const equal = (a, b) => a === undefined ? b === undefined : b !== undefined && Buffer.from(a).equals(Buffer.from(b));
  return {
    ...(encryption ? { encryption } : {}),
    head(key) { const body = objects.get(key); return body ? { size: body.length } : null; },
    get(key, range) { const bytes = objects.get(key); if (!bytes) return null;
      const body = range ? bytes.slice(range.start, range.end + 1) : bytes;
      return { body, size: body.length }; },
    put(key, value) { objects.set(key, value.slice()); },
    delete(key) { objects.delete(key); },
    async putIfAbsent(key, value, _policy, options) {
      const bytes = value instanceof ReadableStream ? new Uint8Array(await new Response(value).arrayBuffer()) : value;
      if (objects.has(key)) return false; objects.set(key, bytes.slice());
      if (options?.routingHint) hints.set(key, options.routingHint);
      return true;
    },
    compareAndSwap(key, expected, value) { if (!equal(expected, objects.get(key))) return false; objects.set(key, value.slice()); return true; },
    compareAndDelete(key, expected) { if (!equal(expected, objects.get(key))) return false; objects.delete(key); return true; },
    list(query) { const paths = [...objects.keys()].filter(key => key.startsWith(query.prefix) && (query.after === undefined || key > query.after)).sort();
      return { paths: paths.slice(0, query.limit), hasMore: paths.length > query.limit }; },
    benchmarkInventory() { return [...objects].map(([key, bytes]) => ({ key, bytes: bytes.length,
      ...(hints.has(key) ? { hint: hints.get(key) } : {}),
      ...(key.endsWith(".idx") ? { index: JSON.parse(new TextDecoder().decode(bytes)) } : {}) })); },
    benchmarkStoredBytes() { let total = 0; for (const bytes of objects.values()) total += bytes.length; return total; },
  };
}
const plain = backend();
const encrypted = backend(fixture.encryption);
const handler = createGraftRemoteHandler({
  authenticate({ request }) { if (request.headers.get("authorization") !== "Bearer benchmark-token") throw new Error("synthetic authorization missing"); return "benchmark"; },
  authorize() {},
  backend: ({ repository }) => repository.namespace !== "bench" ? null : repository.name === "plain" ? plain : repository.name === "encrypted" ? encrypted : null,
  limits: { maxRequestBytes: 64 * 1024 * 1024 },
});
const count = (field) => new Transform({ transform(chunk, _, done) { metrics[field] += chunk.length; done(null, chunk); } });
const server = createServer(async (incoming, outgoing) => {
  metrics.requests += 1;
  try {
    const origin = `http://127.0.0.1:${server.address().port}`;
    const url = new URL(incoming.url, origin);
    const parts = url.pathname.split("/").filter(Boolean).map(decodeURIComponent);
    const operation = fixture.resourceMetrics && parts[2] === "e2ee" ? `${parts[2]}/${parts[3]}` : parts[2] ?? "descriptor";
    metrics.operation_counts[operation] = (metrics.operation_counts[operation] ?? 0) + 1;
    const headers = new Headers();
    for (let i = 0; i < incoming.rawHeaders.length; i += 2) headers.append(incoming.rawHeaders[i], incoming.rawHeaders[i + 1]);
    const request = new Request(url, { method: incoming.method, headers,
      ...(!["GET", "HEAD"].includes(incoming.method) ? { body: Readable.toWeb(incoming.pipe(count("request_body_bytes"))), duplex: "half" } : {}) });
    const configCas = incoming.method === "POST" && url.pathname === "/bench/encrypted/cas/e2ee/config";
    if (cliFault && configCas) cliFault.cas_bodies.push(Buffer.from(await request.clone().arrayBuffer()).toString("hex"));
    if (cliFault && configCas && holdNextConfigDispatch) {
      holdNextConfigDispatch = false;
      cliFault.held_dispatch = true;
      console.log(JSON.stringify({ config_dispatch_held: true }));
      return;
    }
    if (cliFault && configCas && dropNextConfigDispatch) {
      dropNextConfigDispatch = false;
      cliFault.dropped_dispatch = true;
      dropConfigRead = true;
      outgoing.destroy();
      return;
    }
    const response = await handler({ request, route: { namespace: parts[0], repository: parts[1], operation: parts[2], objectPath: parts.slice(3).join("/") || undefined }, adapterContext: undefined });
    if (cliFault && configCas && holdNextConfigCommit && response.status === 204) {
      holdNextConfigCommit = false;
      cliFault.held_commit = true;
      console.log(JSON.stringify({ config_commit_held: true }));
      // The test kills its actual CLI after this signal. Keep the acknowledgement
      // unobservable until its socket closes; no fabricated success response.
      return;
    }
    if (cliFault?.armed && configCas && !cliFault.dropped_ack && response.status === 204) {
      cliFault.dropped_ack = true;
      dropConfigRead = true;
      outgoing.destroy();
      return;
    }
    if (cliFault && dropConfigRead && incoming.method === "GET" && url.pathname === "/bench/encrypted/raw/e2ee/config") {
      dropConfigRead = false;
      cliFault.dropped_read = true;
      outgoing.destroy();
      return;
    }
    outgoing.writeHead(response.status, Object.fromEntries(response.headers));
    if (response.body) Readable.fromWeb(response.body).pipe(count("response_body_bytes")).pipe(outgoing);
    else outgoing.end();
  } catch (error) { console.error(error); outgoing.writeHead(500); outgoing.end(); }
});
server.listen(0, "127.0.0.1", () => console.log(JSON.stringify({ origin: `http://127.0.0.1:${server.address().port}/bench` })));
createInterface({ input: process.stdin }).on("line", line => {
  if (line === "inventory" && fixture.resourceMetrics) {
    console.log(JSON.stringify({ plain: plain.benchmarkInventory(), encrypted: encrypted.benchmarkInventory() }));
    return;
  }
  if (line === "arm-cli-config-dispatch-hold" && cliFault && !cliFault.held_dispatch) {
    holdNextConfigDispatch = true;
    console.log(JSON.stringify({ armed: true }));
    return;
  }
  if (line === "arm-cli-config-dispatch-fault" && cliFault && !cliFault.dropped_dispatch) {
    dropNextConfigDispatch = true;
    console.log(JSON.stringify({ armed: true }));
    return;
  }
  if (line === "arm-cli-config-hold" && cliFault && !cliFault.held_commit) {
    holdNextConfigCommit = true;
    console.log(JSON.stringify({ armed: true }));
    return;
  }
  if (line === "arm-cli-config-fault" && cliFault && !cliFault.armed) {
    cliFault.armed = true;
    console.log(JSON.stringify({ armed: true }));
    return;
  }
  if (line !== "snapshot") throw new Error("unknown benchmark command");
  console.log(JSON.stringify({ ...metrics, ...(cliFault ? { cli_config_fault: cliFault } : {}),
    ...(fixture.resourceMetrics ? { server_cpu: process.cpuUsage(), server_rss_bytes: process.memoryUsage().rss,
      stored_bytes: { plain: plain.benchmarkStoredBytes(), encrypted: encrypted.benchmarkStoredBytes() } } : {}) }));
  metrics = emptyMetrics();
});
process.stdin.on("end", () => { server.closeAllConnections(); server.close(); });
