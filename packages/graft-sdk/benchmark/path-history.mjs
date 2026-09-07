// Native SDK benchmark. Fresh-process samples are OS-cache-warm, not cold-disk claims.
import assert from 'node:assert/strict'
import fs from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
import { spawnSync } from 'node:child_process'
import { performance } from 'node:perf_hooks'
import { createRequire } from 'node:module'
import { fileURLToPath } from 'node:url'
const require = createRequire(import.meta.url)
const { RepositorySession, sdkVersion } = require('..')
const script = fileURLToPath(import.meta.url)

if (process.argv[2] === '--query') {
  const session = await RepositorySession.open(process.argv[3])
  const initial_rss_bytes = process.memoryUsage().rss
  const samples = []
  for (let i = 0; i < 6; i++) {
    const started = performance.now()
    const page = await session.pathHistory({ path: 'rare.txt', maxCommits: 100, maxBytes: 8 * 1024 * 1024 })
    samples.push({ ms: performance.now() - started, matches: page.commits.length, has_more: page.has_more, ...page.telemetry })
    assert.ok(page.telemetry.commits_scanned <= 100)
    assert.ok(page.telemetry.object_bytes_read <= 8 * 1024 * 1024)
  }
  const start = performance.now()
  let rootRevision, cursor, pages = 0, scanned = 0, bytes = 0, matches = 0
  do {
    const page = await session.pathHistory({ path: 'rare.txt', maxCommits: 100, cursor })
    if (page.commits.length) rootRevision = page.commits.at(-1).id
    pages++; scanned += page.telemetry.commits_scanned; bytes += page.telemetry.object_bytes_read; matches += page.commits.length
    cursor = page.next_cursor
    if (!page.has_more) break
    assert.ok(pages <= 10000)
  } while (true)
  const full = { ms: performance.now() - start, pages, scanned, bytes, matches }
  const abort = new AbortController()
  const cancellationStarted = performance.now()
  const operation = session.pathHistory({ path: 'absent.txt', maxCommits: 1000, maxBytes: 64 * 1024 * 1024, signal: abort.signal })
  setTimeout(() => abort.abort(), 2)
  let cancellation
  try { await operation; cancellation = { completed_before_abort: true } }
  catch (error) { assert.equal(error.name, 'AbortError'); cancellation = { ms: performance.now() - cancellationStarted, error: error.name } }
  // Queue a subsequent operation to include worker unwind, not just JS promise rejection.
  const recoveryStart = performance.now()
  await session.pathHistory({ path: 'rare.txt', maxCommits: 1 })
  cancellation.worker_recovered_ms = performance.now() - recoveryStart
  const query_max_rss_kib = process.resourceUsage().maxRSS
  const head = (await session.repositoryMetadata()).current_head
  await fs.writeFile(path.join(process.argv[3], 'file-1.txt'), 'staged other')
  await session.stagePaths({ paths: ['file-1.txt'], expectedHead: head })
  await fs.writeFile(path.join(process.argv[3], 'file-1.txt'), 'external other')
  const stagedBefore = await session.diff({ staged: true })
  const restoreStart = performance.now()
  await session.restorePaths({ paths: ['file-0.txt'], source: rootRevision, expectedHead: head, requireClean: false })
  const restore_ms = performance.now() - restoreStart
  assert.equal(await fs.readFile(path.join(process.argv[3], 'file-1.txt'), 'utf8'), 'external other')
  assert.deepEqual(await session.diff({ staged: true }), stagedBefore)
  assert.equal((await session.repositoryMetadata()).current_head, head)
  await session.close()
  console.log(JSON.stringify({ samples, full, cancellation, restore_ms, initial_rss_bytes, query_max_rss_kib, max_rss_kib: process.resourceUsage().maxRSS }))
} else {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'graft-path-bench-'))
  const scenarios = [
    { name: 'long-sparse', commits: Number(process.env.GRAFT_HISTORY_COMMITS || 1200), paths: 32 },
    { name: 'large-directory', commits: Number(process.env.GRAFT_HISTORY_LARGE_COMMITS || 100), paths: Number(process.env.GRAFT_HISTORY_PATHS || 10000) },
  ]
  const report = { version: sdkVersion(), node: process.version, platform: `${process.platform}-${process.arch}`, cold_definition: 'fresh process; OS file cache not flushed', scenarios: [] }
  try {
    for (const config of scenarios) {
      const dir = path.join(root, config.name); await fs.mkdir(dir)
      const session = await RepositorySession.open(dir); await session.init()
      const start = performance.now()
      await fs.writeFile(path.join(dir, 'rare.txt'), 'rare root')
      for (let i = 0; i < config.paths; i++) await fs.writeFile(path.join(dir, `file-${i}.txt`), 'constant')
      await session.addAll(); await session.commit('root')
      for (let i = 1; i < config.commits; i++) {
        await fs.writeFile(path.join(dir, 'file-0.txt'), `${i}`)
        await session.stagePaths({ paths: ['file-0.txt'] })
        await session.commit(`unrelated ${i}`)
      }
      await session.close()
      const setup_ms = performance.now() - start
      const child = spawnSync(process.execPath, [script, '--query', dir], { encoding: 'utf8', timeout: 300000, maxBuffer: 8 * 1024 * 1024 })
      assert.equal(child.status, 0, child.stderr)
      report.scenarios.push({ ...config, setup_ms, ...JSON.parse(child.stdout) })
    }
    console.log(JSON.stringify(report, null, 2))
  } finally { await fs.rm(root, { recursive: true, force: true }) }
}
