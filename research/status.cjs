const { RepositorySession } = require(process.argv[2]);
const root = '/private/tmp/eidos-sync-perf-20260908/local';
(async () => {
  const repo = await RepositorySession.open(root);
  try {
    await repo.statusIncremental();
    const samples = [];
    for (let i = 0; i < 7; i++) {
      const start = performance.now();
      const result = await repo.statusIncremental();
      samples.push({ ms: performance.now() - start, telemetry: result.telemetry });
    }
    const sorted = samples.map(s => s.ms).sort((a,b) => a-b);
    console.log(JSON.stringify({medianMs: sorted[3], samples}, null, 2));
  } finally { await repo.close(); }
})().catch(e => { console.error(e); process.exitCode = 1; });
