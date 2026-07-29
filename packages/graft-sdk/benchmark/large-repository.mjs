import assert from "node:assert/strict"
import { spawn } from "node:child_process"
import fs from "node:fs/promises"
import os from "node:os"
import path from "node:path"
import { performance } from "node:perf_hooks"
import { createRequire } from "node:module"
import { fileURLToPath } from "node:url"

const require = createRequire(import.meta.url)
const { DatabaseSync } = require("node:sqlite")
const { RepositorySession } = require("..")

const trackedPaths = 46_665
const ignoredNodeModulesPaths = 46_318
const ordinaryPathsBeforeIgnore = 345
const historyCommitsBeforeLargeTree = 49
const fixtureVersion = 1
const iterations = positiveInteger(process.env.GRAFT_SDK_LARGE_ITERATIONS, 7)
const longTimeoutMs = positiveInteger(
  process.env.GRAFT_SDK_LARGE_LONG_TIMEOUT_MS,
  60_000
)
const configuredFixture = process.env.GRAFT_SDK_LARGE_FIXTURE
const temporaryFixture = configuredFixture === undefined
const fixtureRoot = configuredFixture
  ? path.resolve(configuredFixture)
  : await fs.mkdtemp(path.join(os.tmpdir(), "graft-sdk-large-"))
const markerPath = `${fixtureRoot}.fixture-v${fixtureVersion}.json`
const benchmarkFile = fileURLToPath(import.meta.url)

if (process.argv[2] === "--child-operation") {
  await runChildOperation(process.argv[3], path.resolve(process.argv[4]))
  process.exit(0)
}

try {
  await prepareFixture(fixtureRoot, markerPath)
  const report = await runBenchmark(fixtureRoot)
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
} finally {
  if (temporaryFixture) {
    await fs.rm(fixtureRoot, { force: true, recursive: true })
    await fs.rm(markerPath, { force: true })
  }
}

async function prepareFixture(root, marker) {
  if (await fixtureIsReady(root, marker)) return
  await fs.rm(root, { force: true, recursive: true })
  await fs.rm(marker, { force: true })
  await fs.mkdir(root, { recursive: true })

  const session = await RepositorySession.open(root)
  try {
    await session.init()
    createDatabase(path.join(root, "project.eidos"))
    await fs.writeFile(path.join(root, "journal.txt"), "history 0\n")
    await session.addAll()
    await session.commit("fixture history 0")

    for (let index = 1; index < historyCommitsBeforeLargeTree; index += 1) {
      await fs.writeFile(path.join(root, "journal.txt"), `history ${index}\n`)
      await session.addAll()
      await session.commit(`fixture history ${index}`)
    }

    await createLargeTrackedTree(root)
    await session.addAll()
    await session.commit("large tracked tree")

    await fs.writeFile(path.join(root, ".gitignore"), "node_modules/\n")
    await fs.mkdir(path.join(root, "apps", "web"), { recursive: true })
    await fs.writeFile(
      path.join(root, "apps", "web", ".graftignore"),
      "generated/\n"
    )
    await session.addAll()
    await session.commit("ignore generated dependencies")
  } finally {
    await session.close()
  }

  const markerData = {
    fixture_version: fixtureVersion,
    tracked_paths: trackedPaths,
    ignored_node_modules_paths: ignoredNodeModulesPaths,
    history_commits: historyCommitsBeforeLargeTree + 2,
  }
  await fs.writeFile(marker, `${JSON.stringify(markerData, null, 2)}\n`)
}

async function fixtureIsReady(root, marker) {
  try {
    const data = JSON.parse(await fs.readFile(marker, "utf8"))
    return (
      data.fixture_version === fixtureVersion &&
      data.tracked_paths === trackedPaths &&
      (await fs.stat(path.join(root, ".graft"))).isDirectory()
    )
  } catch {
    return false
  }
}

async function createLargeTrackedTree(root) {
  const batch = []
  for (let index = 0; index < ignoredNodeModulesPaths; index += 1) {
    const packageIndex = Math.floor(index / 64)
    const fileIndex = index % 64
    const file = path.join(
      root,
      "node_modules",
      `package-${packageIndex.toString().padStart(4, "0")}`,
      `file-${fileIndex.toString().padStart(2, "0")}.js`
    )
    batch.push(writeFixtureFile(file, "module.exports = 1\n"))
    if (batch.length === 512) await flush(batch)
  }

  const existingOrdinaryPaths = 2
  const generatedPaths = 40
  for (let index = 0; index < generatedPaths; index += 1) {
    const file = path.join(root, "apps", "web", "generated", `item-${index}.txt`)
    batch.push(writeFixtureFile(file, "generated\n"))
  }
  const documentationPaths =
    ordinaryPathsBeforeIgnore - existingOrdinaryPaths - generatedPaths
  for (let index = 0; index < documentationPaths; index += 1) {
    const file = path.join(root, "docs", `note-${index}.md`)
    batch.push(writeFixtureFile(file, "fixture\n"))
    if (batch.length === 512) await flush(batch)
  }
  await flush(batch)
}

async function writeFixtureFile(file, contents) {
  await fs.mkdir(path.dirname(file), { recursive: true })
  await fs.writeFile(file, contents)
}

async function flush(promises) {
  await Promise.all(promises)
  promises.length = 0
}

function createDatabase(databasePath) {
  const database = new DatabaseSync(databasePath)
  try {
    database.exec(
      "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT NOT NULL);" +
        "INSERT INTO items (name) VALUES ('baseline')"
    )
  } finally {
    database.close()
  }
}

function mutateDatabase(databasePath) {
  const database = new DatabaseSync(databasePath)
  try {
    database.exec("INSERT INTO items (name) VALUES ('changed')")
  } finally {
    database.close()
  }
}

async function runBenchmark(root) {
  await restoreFixtureDatabase(root)
  const processColdStatus = await sampleChildOperations(
    iterations,
    "cold-status",
    root,
    longTimeoutMs
  )
  const sessionColdStatus = await sample(iterations, async () => {
    const session = await RepositorySession.open(root)
    try {
      return await session.status()
    } finally {
      await session.close()
    }
  })

  const session = await RepositorySession.open(root)
  if (typeof session.statusIncremental === "function") {
    await session.statusIncremental()
  } else {
    await session.status()
  }
  const hotStatus = await sample(iterations, () => session.status())
  const cleanStatus = hotStatus.values.at(-1)
  assert.equal(cleanStatus.dirty, false)
  const historyOne = await timed(() => session.history({ limit: 1 }))
  const summarySamples =
    typeof session.historySummaries === "function"
      ? await sample(iterations, () => session.historySummaries({ limit: 50 }))
      : undefined
  const ignoreBatchPaths = largeIgnoreBatchPaths()
  const ignoreBatchFirst = await timed(() =>
    session.isIgnoredPaths({ paths: ignoreBatchPaths })
  )
  assert.equal(ignoreBatchFirst.value.paths.length, 1_000)
  assert.equal(ignoreBatchFirst.value.paths[0].is_directory, true)
  assert.equal(ignoreBatchFirst.value.paths[0].has_tracked_descendants, true)
  const ignoreBatchSamples = await sample(iterations, () =>
    session.isIgnoredPaths({ paths: ignoreBatchPaths })
  )
  const inventoryFirst = await timed(() =>
    session.inventory({ kind: "tracked_ignored", limit: 5 })
  )
  const inventorySamples = await sample(iterations, () =>
    session.inventory({ kind: "tracked_ignored", limit: 5 })
  )
  assert.equal(inventoryFirst.value.total_matching, 46_358)
  assert.equal(inventorySamples.values.at(-1).telemetry.paths_examined, 0)

  mutateDatabase(path.join(root, "project.eidos"))
  const changedStatus =
    typeof session.statusIncremental === "function"
      ? await timed(() => session.statusIncremental())
      : await timed(() => session.status())
  const pathDiffSamples =
    typeof session.diffPaths === "function"
      ? await sample(iterations, () =>
          session.diffPaths({ paths: ["project.eidos"], rows: true, limit: 10 })
        )
      : await sample(iterations, () =>
          session.diff({ path: "project.eidos", rows: true })
        )
  await session.close()

  const fullDiff = await runTimedChild("working-diff", root, longTimeoutMs)
  const cancellation = await runTimedChild("cancellation", root, longTimeoutMs)

  return {
    schema: "graft-sdk-large-repository-benchmark-v1",
    generated_at: new Date().toISOString(),
    fixture: {
      tracked_paths: trackedPaths,
      ignored_node_modules_paths: ignoredNodeModulesPaths,
      nested_ignore: "apps/web/.graftignore",
      changed_path: "project.eidos",
      history_commits: historyCommitsBeforeLargeTree + 2,
    },
    environment: {
      platform: process.platform,
      arch: process.arch,
      node: process.version,
      iterations,
    },
    milliseconds: {
      status_process_cold: summarize(processColdStatus.milliseconds),
      status_session_cold: summarize(sessionColdStatus.milliseconds),
      status_hot: summarize(hotStatus.milliseconds),
      history_legacy_limit_1: round(historyOne.milliseconds),
      history_summaries_50: summarySamples
        ? summarize(summarySamples.milliseconds)
        : null,
      ignore_batch_1000_first: round(ignoreBatchFirst.milliseconds),
      ignore_batch_1000_hot: summarize(ignoreBatchSamples.milliseconds),
      tracked_ignored_inventory_first: round(inventoryFirst.milliseconds),
      tracked_ignored_inventory_hot: summarize(inventorySamples.milliseconds),
      changed_status: round(changedStatus.milliseconds),
      changed_path_diff: summarize(pathDiffSamples.milliseconds),
      working_diff: fullDiff.milliseconds,
      cancellation_rejection: cancellation.result?.cancellation_rejection_ms ?? null,
      cancellation_session_reusable:
        cancellation.result?.cancellation_session_reusable_ms ?? null,
    },
    bytes: {
      status_response: jsonBytes(cleanStatus),
      history_legacy_limit_1_response: jsonBytes(historyOne.value),
      history_summaries_50_response: summarySamples
        ? jsonBytes(summarySamples.values.at(-1))
        : null,
      ignore_batch_1000_request: jsonBytes({ paths: ignoreBatchPaths }),
      ignore_batch_1000_response: jsonBytes(ignoreBatchSamples.values.at(-1)),
      tracked_ignored_inventory_response: jsonBytes(
        inventorySamples.values.at(-1)
      ),
      changed_status_response: jsonBytes(changedStatus.value),
      changed_path_diff_request: jsonBytes({
        paths: ["project.eidos"],
        rows: true,
        limit: 10,
      }),
      changed_path_diff_response: jsonBytes(pathDiffSamples.values.at(-1)),
      working_diff_response: fullDiff.result?.response_bytes ?? null,
    },
    rss: {
      parent_peak_bytes: peakRssBytes(),
      process_cold_status_peak_bytes: Math.max(
        ...processColdStatus.values.map((value) => value.peak_rss_bytes)
      ),
      working_diff_peak_bytes: fullDiff.result?.peak_rss_bytes ?? null,
      cancellation_peak_bytes: cancellation.result?.peak_rss_bytes ?? null,
    },
    telemetry: {
      status: changedStatus.value.telemetry ?? null,
      diff: pathDiffSamples.values.at(-1).telemetry ?? null,
      history: summarySamples?.values.at(-1)?.telemetry ?? null,
      ignore_batch: ignoreBatchSamples.values.at(-1).telemetry,
      inventory: {
        first: inventoryFirst.value.telemetry,
        hot: inventorySamples.values.at(-1).telemetry,
        kind: inventorySamples.values.at(-1).kind,
        total_matching: inventorySamples.values.at(-1).total_matching,
        has_more: inventorySamples.values.at(-1).has_more,
        migration: inventorySamples.values.at(-1).migration,
      },
    },
    timeouts: {
      working_diff: fullDiff.timedOut,
      cancellation: cancellation.timedOut,
      long_operation_timeout_ms: longTimeoutMs,
    },
  }
}

async function restoreFixtureDatabase(root) {
  const session = await RepositorySession.open(root)
  try {
    if (typeof session.restorePaths === "function") {
      await session.restorePaths({ source: "HEAD", paths: ["project.eidos"] })
    } else {
      await session.restore({ source: "HEAD", path: "project.eidos" })
    }
  } finally {
    await session.close()
  }
}

async function runChildOperation(operation, root) {
  const session = await RepositorySession.open(root)
  try {
    if (operation === "cold-status") {
      const measured = await timed(() => session.status())
      writeChildResult({
        operation_milliseconds: round(measured.milliseconds),
        response_bytes: jsonBytes(measured.value),
        peak_rss_bytes: peakRssBytes(),
      })
      return
    }
    if (operation === "working-diff") {
      const measured = await timed(() => session.diff({ rows: true }))
      writeChildResult({
        response_bytes: jsonBytes(measured.value),
        peak_rss_bytes: peakRssBytes(),
      })
      return
    }
    if (operation === "cancellation") {
      if (typeof session.statusIncremental === "function") {
        await session.statusIncremental()
      }
      const controller = new AbortController()
      const started = performance.now()
      const pending = session.diff({ rows: true, signal: controller.signal })
      setTimeout(() => controller.abort(), 50)
      let rejectedAt
      try {
        await pending
      } catch (error) {
        assert.equal(error.name, "AbortError")
        rejectedAt = performance.now()
      }
      assert.notEqual(rejectedAt, undefined)
      if (typeof session.statusIncremental === "function") {
        await session.statusIncremental()
      } else {
        await session.status()
      }
      writeChildResult({
        cancellation_rejection_ms: round(rejectedAt - started),
        cancellation_session_reusable_ms: round(performance.now() - rejectedAt),
        peak_rss_bytes: peakRssBytes(),
      })
      return
    }
    throw new Error(`unknown child operation: ${operation}`)
  } finally {
    await session.close()
  }
}

function largeIgnoreBatchPaths() {
  const paths = ["node_modules"]
  for (let index = 0; index < 999; index += 1) {
    const packageIndex = Math.floor(index / 64)
    const fileIndex = index % 64
    paths.push(
      path.join(
        "node_modules",
        `package-${packageIndex.toString().padStart(4, "0")}`,
        `file-${fileIndex.toString().padStart(2, "0")}.js`
      )
    )
  }
  return paths
}

function writeChildResult(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`)
}

async function runTimedChild(operation, root, timeoutMs) {
  const started = performance.now()
  const child = spawn(
    process.execPath,
    [benchmarkFile, "--child-operation", operation, root],
    { stdio: ["ignore", "pipe", "pipe"] }
  )
  const chunks = []
  const errors = []
  child.stdout.on("data", (chunk) => chunks.push(chunk))
  child.stderr.on("data", (chunk) => errors.push(chunk))
  let timedOut = false
  const timeout = setTimeout(() => {
    timedOut = true
    child.kill("SIGKILL")
  }, timeoutMs)
  const code = await new Promise((resolve) => child.on("close", resolve))
  clearTimeout(timeout)
  const milliseconds = round(performance.now() - started)
  if (timedOut) return { milliseconds, timedOut, result: null }
  if (code !== 0) {
    throw new Error(
      `benchmark child ${operation} failed (${code}): ${Buffer.concat(errors)}`
    )
  }
  return {
    milliseconds,
    timedOut,
    result: JSON.parse(Buffer.concat(chunks).toString("utf8")),
  }
}

async function sample(count, operation) {
  const milliseconds = []
  const values = []
  for (let index = 0; index < count; index += 1) {
    const measured = await timed(operation)
    milliseconds.push(measured.milliseconds)
    values.push(measured.value)
  }
  return { milliseconds, values }
}

async function sampleChildOperations(count, operation, root, timeoutMs) {
  const milliseconds = []
  const values = []
  for (let index = 0; index < count; index += 1) {
    const child = await runTimedChild(operation, root, timeoutMs)
    assert.equal(child.timedOut, false)
    milliseconds.push(child.result.operation_milliseconds)
    values.push(child.result)
  }
  return { milliseconds, values }
}

async function timed(operation) {
  const started = performance.now()
  const value = await operation()
  return { milliseconds: performance.now() - started, value }
}

function summarize(values) {
  const sorted = values.toSorted((left, right) => left - right)
  return {
    first: round(values[0]),
    min: round(sorted[0]),
    p50: round(percentile(sorted, 0.5)),
    p95: round(percentile(sorted, 0.95)),
    max: round(sorted.at(-1)),
    mean: round(values.reduce((total, value) => total + value, 0) / values.length),
  }
}

function percentile(sorted, quantile) {
  const index = Math.min(sorted.length - 1, Math.floor(sorted.length * quantile))
  return sorted[index]
}

function positiveInteger(value, fallback) {
  if (value === undefined) return fallback
  const parsed = Number.parseInt(value, 10)
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error("benchmark settings must be positive integers")
  }
  return parsed
}

function jsonBytes(value) {
  return Buffer.byteLength(JSON.stringify(value))
}

function peakRssBytes() {
  const maxRss = process.resourceUsage().maxRSS
  return maxRss * 1024
}

function round(value) {
  return Math.round(value * 1000) / 1000
}
