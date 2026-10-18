import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";
import { assertPrivacyEvidence, readPrivacyEvidence } from "./privacy-evidence.mjs";

for (const enabled of [false, true]) test(`actual service privacy capture opt-in=${enabled}`, { timeout: 15000 }, async (context) => {
  const directory = mkdtempSync(join(tmpdir(), "graft-privacy-service-"));
  const fixture = join(directory, "fixture.json");
  const capture = join(directory, "captures.json");
  const marker = "synthetic leaked config bytes 71abc";
  writeFileSync(fixture, JSON.stringify({ privacyEvidence: enabled,
    encryption: { profile: "encrypted-remote-v1", version: 1, suite: 1, uuid: "00010203-0405-4607-8809-0a0b0c0d0e0f" },
    objects: [["e2ee/config", [...Buffer.from(marker)]]], serviceLifetimeMs: 10000 }));
  const code = `${enabled ? "" : 'Response.prototype.clone = () => { throw new Error("Opt-out cloned a response"); };'}
    process.argv = [process.execPath, "tests/e2ee/remote-service.mjs", ...process.argv.slice(1)];
    await import("./tests/e2ee/remote-service.mjs");`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", code, fixture, capture], { stdio: ["ignore", "pipe", "pipe", "ipc"] });
  const closed = once(child, "close");
  let errors = "";
  child.stderr.on("data", (chunk) => { errors += chunk; });
  const lines = createInterface({ input: child.stdout });
  try {
    const [line] = await Promise.race([once(lines, "line", { signal: context.signal }), closed.then(() => {
      throw new Error(`Synthetic service failed before readiness: ${errors}`);
    })]);
    const ready = JSON.parse(line);
    const response = await fetch(ready.url + "/raw/e2ee/config", { headers: {
      authorization: "Bearer account-token", "Graft-Protocol": "1" } });
    assert.equal(response.status, 200, errors);
    assert.equal(await response.text(), marker);
    const legacy = JSON.parse(readFileSync(capture));
    assert.ok(Array.isArray(legacy) && legacy.length === 1);
    assert.equal(legacy[0].responseStatus, 200);
    child.send("graft-e2ee-shutdown");
    const [code] = await closed;
    if (enabled) {
      assert.equal(code, 0, errors);
      const records = readPrivacyEvidence(capture + ".privacy.jsonl");
      for (const surface of ["request", "response", "storage"])
        assert.ok(records.some((record) => record.surface === surface));
      assert.throws(() => assertPrivacyEvidence(records, [marker]), /Private marker/);
    } else {
      assert.equal(existsSync(capture + ".privacy.jsonl"), false);
      assert.equal(errors.includes("Opt-out cloned a response"), false);
    }
  } finally {
    lines.close();
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
    }
    await closed;
    rmSync(directory, { recursive: true, force: true });
  }
});
