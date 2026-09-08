const { RepositorySession } = require(process.argv[2]);
const root = process.argv[3] || '/private/tmp/eidos-sync-perf-20260908/local';
if (!root.startsWith('/private/tmp/eidos-sync-perf-')) throw new Error('Isolated fixture only');
(async () => {
  const repo = await RepositorySession.open(root);
  const fs = require('node:fs');
  const refPath = root + '/.graft/refs/remotes/origin/main';
  const savedRef = fs.readFileSync(refPath);
  try {
    if (process.env.PERF_AHEAD === '1') {
      const { commits } = await repo.history({limit: 1});
      fs.writeFileSync(refPath, commits[0].parents[0] + '\n');
    }
    await repo.statusIncremental();
    const samples = [];
    for (let i = 0; i < 7; i++) {
      const start = performance.now();
      const result = await repo.statusIncremental();
      samples.push({ ms: performance.now() - start, telemetry: result.telemetry, ahead: result.status.ahead, behind: result.status.behind });
    }
    const sorted = samples.map(s => s.ms).sort((a,b) => a-b);
    console.log(JSON.stringify({medianMs: sorted[3], samples}, null, 2));
  } finally { fs.writeFileSync(refPath, savedRef); await repo.close(); }
})().catch(e => { console.error(e); process.exitCode = 1; });
