// Plain HTTP counterpart to node-transfer-progress.mjs; real native SDK + remote.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { once } from 'node:events';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';
import { createInterface } from 'node:readline';
import { DatabaseSync } from 'node:sqlite';
const { RepositorySession } = createRequire(import.meta.url)('../../packages/graft-sdk');
const directory = mkdtempSync(join(tmpdir(), 'graft-plain-progress-'));
const fixture = join(directory, 'fixture.json');
writeFileSync(fixture, JSON.stringify({ objects: [], serviceLifetimeMs: 180000 }));
const service = spawn(process.execPath, ['tests/e2ee/remote-service.mjs', fixture, join(directory, 'capture.json')], { stdio: ['ignore', 'pipe', 'inherit'] });
const lines = createInterface({ input: service.stdout });
const ready = JSON.parse((await once(lines, 'line'))[0]);
const url = ready.url.replace('http://', 'graft+http://');
const sourcePath = join(directory, 'source'); const peerPath = join(directory, 'peer');
mkdirSync(sourcePath); mkdirSync(peerPath);
const source = await RepositorySession.open(sourcePath); const peer = await RepositorySession.open(peerPath);
const reports = [];
async function observe(label, operation) {
  const stages = []; let settled = false;
  const start = performance.now();
  await operation(process.env.GRAFT_PROGRESS_SYNC === '0' ? undefined : (event) => {
    assert.equal(settled, false); stages.push(event);
  });
  settled = true;
  if (process.env.GRAFT_PROGRESS_SYNC !== '0') {
    assert.equal(stages[0]?.phase, 'preparing'); assert.equal(stages.at(-1)?.phase, 'completed');
    assert.ok(stages.some((event) => event.transfer?.transferredBytes > 0));
    assert.ok(stages.some((event) => event.transfer?.totalIsFinal));
    if (['clone', 'pull'].includes(label)) assert.ok(stages.some((event) => event.phase === 'applying'));
    const attempts = new Map();
    for (const { requestId, transfer } of stages) if (transfer) {
      assert.ok(transfer.transferredBytes >= (attempts.get(requestId) ?? 0));
      if (transfer.totalIsFinal) assert.ok(transfer.transferredBytes <= transfer.totalBytes);
      attempts.set(requestId, transfer.transferredBytes);
    }
  }
  reports.push({ label, elapsedMs: Math.round(performance.now() - start) });
}
try {
  await source.init();
  const db = new DatabaseSync(join(sourcePath, 'records.sqlite'));
  db.exec('CREATE TABLE records(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO records VALUES(1, \'first\')'); db.close();
  const payload = randomBytes(1024 * 1024); writeFileSync(join(sourcePath, 'attachment.bin'), payload);
  await source.addAll(); await source.commit('first');
  await source.configureRemote({ name: 'origin', url, bearerToken: 'account-token', upstreamBranch: 'main' });
  await observe('initial-upload', (onSyncProgress) => source.push({ onSyncProgress }));
  await observe('clone', (onSyncProgress) => peer.cloneRepository({ remoteUrl: url, bearerToken: 'account-token', onSyncProgress }));
  assert.deepEqual(readFileSync(join(peerPath, 'attachment.bin')), payload);
  const updated = randomBytes(1024 * 1024); writeFileSync(join(sourcePath, 'update.bin'), updated);
  await source.addAll(); await source.commit('second');
  await observe('push', (onSyncProgress) => source.push({ onSyncProgress }));
  await observe('pull', (onSyncProgress) => peer.pull({ onSyncProgress }));
  assert.deepEqual(readFileSync(join(peerPath, 'update.bin')), updated);
  const received = new DatabaseSync(join(peerPath, 'records.sqlite'), { readOnly: true });
  assert.equal(received.prepare('SELECT value FROM records').get().value, 'first'); received.close();
  console.log(JSON.stringify({ status: 'passed', reports }, null, 2));
} finally {
  await peer.close(); await source.close(); service.kill(); await once(service, 'exit'); lines.close();
  rmSync(directory, { recursive: true, force: true });
}
