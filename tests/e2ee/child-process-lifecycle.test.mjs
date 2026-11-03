import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter, once } from "node:events";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as delay } from "node:timers/promises";
import test from "node:test";
import { stopChildProcess } from "./child-process-lifecycle.mjs";

test("cleanup settles for a child that exited before cleanup", async () => {
  const child = new EventEmitter();
  child.exitCode = 0;
  child.signalCode = null;
  child.kill = () => assert.fail("must not kill an exited child");
  await stopChildProcess(child);
});

test("cleanup registers exit before an immediate termination", async () => {
  const child = new EventEmitter();
  child.exitCode = child.signalCode = null;
  child.kill = () => {
    child.signalCode = "SIGTERM";
    child.emit("exit", null, "SIGTERM");
  };
  await stopChildProcess(child);
});

test("parent-owned remote survives its legacy deadline and exits on IPC disconnect", { timeout: 10000 }, async () => {
  const directory = mkdtempSync(join(tmpdir(), "graft-service-lifetime-"));
  const fixture = join(directory, "fixture.json");
  writeFileSync(fixture, JSON.stringify({ objects: [], serviceLifetime: "parent", serviceLifetimeMs: 20 }));
  const child = spawn(process.execPath, ["tests/e2ee/remote-service.mjs", fixture, join(directory, "capture.json")], {
    stdio: ["ignore", "pipe", "inherit", "ipc"],
  });
  const exited = once(child, "exit");
  const lines = createInterface({ input: child.stdout });
  try {
    const ready = JSON.parse((await once(lines, "line"))[0]);
    await delay(100);
    assert.equal(child.exitCode, null, "service expired while parent was alive");
    const response = await fetch(ready.enrollmentUrl, { headers: { authorization: "Bearer account-token", "Graft-Protocol": "1" } });
    await response.arrayBuffer();
    assert.equal(response.status, 200);
    child.disconnect();
    assert.deepEqual(await exited, [0, null]);
  } finally {
    await stopChildProcess(child);
    lines.close();
    rmSync(directory, { recursive: true, force: true });
  }
});
