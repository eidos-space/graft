import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, readFileSync, existsSync, statSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomBytes } from "node:crypto";
import { runInNewContext } from "node:vm";
import { createService } from "../setup.mjs";

const origin = "http://127.0.0.1:8787";
async function fixture(t, options = {}) {
  const directory = mkdtempSync(join(tmpdir(), "graft-setup-"));
  let service = await createService({ directory, origin, env: {}, ...options });
  t.after(() => {
    service.close();
    rmSync(directory, { recursive: true });
  });
  return {
    directory,
    claim: () => readFileSync(join(directory, "setup-token"), "utf8"),
    request: (path, init = {}) => service.request(new Request(origin + path, init)),
    async restart() {
      service.close();
      service = await createService({ directory, origin, env: {}, ...options });
    },
  };
}
function post(token, value = { mode: "local" }, extra = {}) {
  return {
    method: "POST",
    body: JSON.stringify(value),
    headers: {
      Origin: origin,
      Authorization: `Bearer ${token}`,
      "Content-Type": "application/json",
      ...extra,
    },
  };
}

test("new server exposes setup, not sync; claim survives restart without leaking into HTML", async (t) => {
  const s = await fixture(t);
  const claim = s.claim();
  assert.equal(statSync(join(s.directory, "setup-token")).mode & 0o777, 0o600);
  assert.equal((await s.request("/livez")).status, 200);
  assert.equal((await s.request("/healthz")).status, 503);
  assert.equal((await s.request("/api/graft/repositories")).status, 503);
  const page = await s.request("/setup");
  assert.equal(page.headers.get("cache-control"), "no-store");
  assert.match(page.headers.get("content-security-policy"), /frame-ancestors 'none'/);
  assert.ok(!(await page.text()).includes(claim));
  await s.restart();
  assert.equal(s.claim(), claim);
});

test("setup rejects unauthenticated, cross-origin, null-origin, large and malformed submissions", async (t) => {
  const s = await fixture(t);
  assert.equal((await s.request("/setup", post("wrong"))).status, 401);
  for (const Origin of ["null", "https://evil.example", ""])
    assert.equal((await s.request("/setup", post(s.claim(), undefined, { Origin }))).status, 403);
  assert.equal((await s.request("/setup", post(s.claim(), { mode: "invalid" }))).status, 400);
  assert.equal(
    (await s.request("/setup", post(s.claim(), { mode: "local", pad: "x".repeat(17000) }))).status,
    413,
  );
  assert.equal((await s.request("/setup", { ...post(s.claim()), body: "{" })).status, 400);
  assert.ok(!existsSync(join(s.directory, "server-config.json")));
});

test("local setup creates one persistent key, rejects replay, authenticates sync after restart", async (t) => {
  const s = await fixture(t);
  const claim = s.claim();
  const response = await s.request("/setup", post(claim));
  assert.equal(response.status, 200);
  const { accessKey } = await response.json();
  assert.match(accessKey, /^[a-f0-9]{64}$/);
  assert.equal(statSync(join(s.directory, "server-config.json")).mode & 0o777, 0o600);
  assert.ok(!existsSync(join(s.directory, "setup-token")));
  assert.equal((await s.request("/setup", post(claim))).status, 409);
  assert.equal((await s.request("/healthz")).status, 200);
  const auth = { headers: { Authorization: `Bearer ${accessKey}` } };
  assert.equal((await s.request("/api/graft/repositories", auth)).status, 200);
  assert.equal((await s.request("/api/graft/repositories")).status, 401);
  await s.restart();
  assert.equal((await s.request("/api/graft/repositories", auth)).status, 200);
  assert.equal((await s.request("/setup", post(claim))).status, 409);
  assert.ok(!(await (await s.request("/setup")).text()).includes(accessKey));
});

test("concurrent setup is serialized and failed storage checks can be retried", async (t) => {
  let release;
  let entered;
  const started = new Promise((resolve) => {
    entered = resolve;
  });
  const s = await fixture(t, {
    probe: async () => {
      entered();
      await new Promise((resolve) => {
        release = resolve;
      });
    },
  });
  const claim = s.claim();
  const pending = s.request("/setup/test", post(claim));
  await started;
  assert.equal((await s.request("/setup", post(claim))).status, 409);
  release();
  assert.equal((await pending).status, 200);
  assert.ok(!existsSync(join(s.directory, "server-config.json")));
});

test("S3 form validates and passes credentials only to authenticated probe", async (t) => {
  let config;
  const s = await fixture(t, {
    probe: async (value) => {
      config = value;
      throw new Error("Storage check failed");
    },
  });
  const input = {
    mode: "s3",
    endpoint: "https://s3.example.com",
    region: "region",
    bucket: "graft-test",
    accessKeyId: "access",
    secretAccessKey: "secret",
    pathStyle: false,
  };
  assert.equal((await s.request("/setup/test", post(s.claim(), input))).status, 400);
  assert.equal(config.forcePathStyle, false);
  assert.equal(config.credentials.secretAccessKey, "secret");
  assert.ok(!existsSync(join(s.directory, "server-config.json")));
  config = undefined;
  assert.equal(
    (
      await s.request(
        "/setup/test",
        post(s.claim(), { ...input, endpoint: "http://s3.example.com" }),
      )
    ).status,
    400,
  );
  assert.equal(config, undefined);
});

test("legacy key-file installations bypass setup and missing configuration fails closed", async (t) => {
  const directory = mkdtempSync(join(tmpdir(), "graft-legacy-"));
  t.after(() => rmSync(directory, { recursive: true }));
  const key = join(directory, "legacy-key");
  writeFileSync(key, randomBytes(32).toString("hex"));
  const service = await createService({ directory, origin, env: { GRAFT_ACCESS_KEY_FILE: key } });
  assert.equal(service.initialized, true);
  assert.equal((await service.request(new Request(origin + "/setup", post("wrong")))).status, 409);
  service.close();
  await assert.rejects(createService({ directory, origin, env: {} }), /Existing data/);
  assert.ok(!existsSync(join(directory, "setup-token")));
});

test("setup status requires the same credential and Origin without probing or consuming setup", async (t) => {
  let probes = 0;
  const s = await fixture(t, { probe: async () => { probes += 1; } });
  const claim = s.claim();
  assert.equal((await s.request("/setup/status")).status, 405);
  for (const path of ["/setup/status", "/setup/test", "/setup"]) {
    for (const token of ["", "wrong"])
      assert.equal((await s.request(path, post(token))).status, 401);
    for (const Origin of ["null", "https://evil.example", ""])
      assert.equal((await s.request(path, post(claim, undefined, { Origin }))).status, 403);
  }
  for (let i = 0; i < 2; i++) {
    const response = await s.request("/setup/status", {
      method: "POST", headers: { Origin: origin, Authorization: `Bearer ${claim}` },
    });
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("cache-control"), "no-store");
    assert.deepEqual(await response.json(), { status: "authorized" });
  }
  assert.equal(probes, 0);
  assert.equal(s.claim(), claim);
  assert.ok(!existsSync(join(s.directory, "graft.sqlite")));
  assert.equal((await s.request("/setup", post(claim))).status, 200);
  for (const path of ["/setup/status", "/setup/test", "/setup"])
    assert.equal((await s.request(path, post(claim))).status, 409);
});

// Run the shipped inline controller with a minimal browser API. HTTP decisions
// come from createService, not canned status responses. Real layout and browser
// Origin behavior are checked separately in an isolated browser fixture.
async function pageController(s, token = "", options = {}) {
  const html = await (await s.request("/setup")).text();
  const elements = new Map();
  for (const match of html.matchAll(/<[^>]+\bid="([^"]+)"[^>]*>/g)) {
    elements.set(match[1], {
      hidden: /\bhidden\b/.test(match[0]), disabled: /\bdisabled\b/.test(match[0]),
      value: match[0].match(/\bvalue="([^"]*)"/)?.[1] ?? "", textContent: "",
      setAttribute() {}, reportValidity: () => true, reset() {}, focus() {}, select() {},
    });
  }
  elements.get("mode").value = "local";
  const requests = [];
  const location = { hash: token ? `#token=${token}` : "", pathname: "/setup" };
  const context = {
    document: { documentElement: {}, getElementById: (id) => elements.get(id), querySelectorAll: () => [] },
    navigator: { language: "en" }, location, URLSearchParams,
    history: { replaceState() { location.hash = ""; } },
    fetch: async (path, init) => {
      requests.push({ path, init });
      if (options.fetch) return options.fetch(path, init);
      return s.request(path, { ...init, headers: { Origin: origin, ...init.headers } });
    },
  };
  runInNewContext(html.match(/<script[^>]*>([\s\S]*?)<\/script>/)[1], context);
  const settle = async () => { for (let i = 0; i < 8; i++) await new Promise(setImmediate); };
  await settle();
  return { elements, requests, location, settle, run: (source) => runInNewContext(source, context) };
}

test("entry controller keeps missing and rejected links out of the storage form across language and mode changes", async (t) => {
  const s = await fixture(t);
  for (const [token, heading] of [["", "Open your initialization link"], ["wrong", "This initialization link is not valid"]]) {
    const p = await pageController(s, token);
    assert.equal(p.elements.get("form").hidden, true);
    assert.equal(p.elements.get("setup-fields").disabled, true);
    assert.equal(p.elements.get("heading").textContent, heading);
    assert.match(p.elements.get("command").textContent, /manage\.mjs setup-link$/);
    assert.match(p.elements.get("entry-help").textContent, /your server.*Compose/);
    p.elements.get("mode").value = "s3";
    p.elements.get("mode").onchange();
    p.elements.get("language").value = "zh";
    p.elements.get("language").onchange();
    assert.equal(p.elements.get("form").hidden, true);
    assert.equal(p.elements.get("s3-fields").disabled, true);
    assert.match(p.elements.get("entry-help").textContent, /服务器.*Compose/);
    assert.match(p.elements.get("entry-next").textContent, /完整网址/);
    assert.equal(p.requests.length, token ? 1 : 0);
    assert.equal(p.location.hash, "");
  }
});

test("entry controller waits for server verification and offers retry without exposing fields", async (t) => {
  const s = await fixture(t);
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const options = { fetch: () => pending };
  const p = await pageController(s, s.claim(), options);
  assert.equal(p.elements.get("form").hidden, true);
  assert.match(p.elements.get("heading").textContent, /Verifying/);
  p.elements.get("language").value = "zh";
  p.elements.get("language").onchange();
  assert.equal(p.elements.get("form").hidden, true);
  release(new Response("unavailable", { status: 503 }));
  await p.settle();
  assert.equal(p.elements.get("form").hidden, true);
  assert.equal(p.elements.get("retry").hidden, false);
  assert.match(p.elements.get("heading").textContent, /无法验证/);
  options.fetch = (path, init) => s.request(path, { ...init, headers: { Origin: origin, ...init.headers } });
  p.elements.get("retry").onclick();
  await p.settle();
  assert.equal(p.elements.get("form").hidden, false);
});

test("entry controller recovers from Origin rejection and closes a stale authorized form", async (t) => {
  const s = await fixture(t);
  const claim = s.claim();
  const rejected = await pageController(s, claim, {
    fetch: (path, init) => s.request(path, { ...init, headers: { Origin: "https://evil.example", ...init.headers } }),
  });
  assert.equal(rejected.elements.get("form").hidden, true);
  assert.match(rejected.elements.get("heading").textContent, /configured address/);
  assert.match(rejected.elements.get("intro").textContent, /GRAFT_PUBLIC_ORIGIN/);
  const stale = await pageController(s, claim);
  assert.equal((await s.request("/setup", post(claim))).status, 200);
  await stale.run("submit(true)");
  assert.equal(stale.elements.get("form").hidden, true);
  assert.equal(stale.elements.get("setup-fields").disabled, true);
  assert.equal(stale.elements.get("heading").textContent, "Setup is complete");
  assert.equal(stale.elements.get("key").value, "");
  assert.match(stale.elements.get("command").textContent, /access-key$/);
});

test("verified controller can correct a failed check, complete setup, and shows recovery on reopening", async (t) => {
  let failing = true;
  const s = await fixture(t, { probe: async () => { if (failing) throw new Error("Correct storage details"); } });
  const claim = s.claim();
  const p = await pageController(s, claim);
  assert.equal(p.elements.get("form").hidden, false);
  assert.equal(p.elements.get("setup-fields").disabled, false);
  assert.equal(p.requests[0].path, "/setup/status");
  await p.run("submit(true)");
  assert.equal(p.elements.get("message").textContent, "Correct storage details");
  assert.equal(p.elements.get("form").hidden, false);
  assert.equal(p.elements.get("test").disabled, false);
  failing = false;
  await p.run("submit(true)");
  assert.match(p.elements.get("message").textContent, /successful/);
  await p.run("submit(false)");
  assert.equal(p.elements.get("form").hidden, true);
  assert.equal(p.elements.get("result").hidden, false);
  assert.match(p.elements.get("key").value, /^[a-f0-9]{64}$/);
  const reopened = await pageController(s, claim);
  assert.equal(reopened.requests.length, 0);
  assert.equal(reopened.elements.get("form").hidden, true);
  assert.equal(reopened.elements.get("heading").textContent, "Setup is complete");
  assert.match(reopened.elements.get("command").textContent, /manage\.mjs access-key$/);
  assert.equal(reopened.elements.get("key").value, "");
});
