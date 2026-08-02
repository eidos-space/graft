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
const sdkModule = process.env.GRAFT_SDK_MODULE ?? ".."
const sdkRoot = path.resolve(import.meta.dirname, sdkModule)
const sdkVersion = require(path.join(sdkRoot, "package.json")).version
const { RepositorySession } = require(sdkRoot)
const benchmarkFile = fileURLToPath(import.meta.url)
const profile = process.env.GRAFT_PERF_PROFILE ?? "full"
const iterations = positiveInteger(process.env.GRAFT_PERF_ITERATIONS, 5)

if (process.argv[2] === "--child") {
  const kind = process.argv[3]
  const scenario = JSON.parse(process.argv[4])
  const result =
    kind === "files"
      ? await runFileScenario(scenario)
      : await runSqliteScenario(scenario)
  process.stdout.write(`${JSON.stringify(result)}\n`)
  process.exit(0)
}

const output = path.resolve(
  process.env.GRAFT_PERF_OUTPUT ??
    path.join(import.meta.dirname, "results", "performance-matrix-macos-arm64.json")
)
const selected = profiles(profile)
const started = performance.now()
const fileScenarios = []
const sqliteScenarios = []

for (const scenario of selected.files) {
  process.stderr.write(
    `files: ${scenario.file_count.toLocaleString()} paths (${scenario.change_counts.join(", ")} changed)\n`
  )
  fileScenarios.push(await runChild("files", scenario))
}
for (const scenario of selected.sqlite) {
  process.stderr.write(
    `sqlite: ${scenario.rows.toLocaleString()} rows, ${scenario.payload_bytes} byte payloads\n`
  )
  sqliteScenarios.push(await runChild("sqlite", scenario))
}

const report = {
  schema: "graft-sdk-performance-matrix-v1",
  generated_at: new Date().toISOString(),
  source_revision: await gitRevision(),
  sdk: { module: sdkRoot, version: sdkVersion },
  profile,
  methodology: {
    iterations,
    timing: "wall-clock milliseconds via performance.now()",
    percentiles: "nearest-rank over repeated resident-session reads",
    fixture_generation: "reported separately and excluded from Graft operation timings",
    isolation: "one fresh repository and Node.js child process per scale point",
  },
  environment: {
    platform: process.platform,
    arch: process.arch,
    node: process.version,
    cpu: os.cpus()[0]?.model ?? "unknown",
    cpu_count: os.cpus().length,
    memory_bytes: os.totalmem(),
    os: `${os.type()} ${os.release()}`,
  },
  total_wall_milliseconds: round(performance.now() - started),
  files: fileScenarios,
  sqlite: sqliteScenarios,
}
await fs.mkdir(path.dirname(output), { recursive: true })
await fs.writeFile(output, `${JSON.stringify(report, null, 2)}\n`)
process.stderr.write(`wrote ${output}\n`)

function profiles(name) {
  if (name === "smoke") {
    return {
      files: [{ file_count: 100, file_bytes: 256, change_counts: [1, 10] }],
      sqlite: [{ rows: 1_000, payload_bytes: 64, change_counts: [1, 100] }],
    }
  }
  if (name !== "full") throw new Error(`unknown profile: ${name}`)
  return {
    files: [
      { file_count: 100, file_bytes: 256, change_counts: [1, 10, 100] },
      { file_count: 1_000, file_bytes: 256, change_counts: [1, 100, 1_000] },
      { file_count: 10_000, file_bytes: 256, change_counts: [1, 100, 1_000] },
      { file_count: 50_000, file_bytes: 256, change_counts: [1, 100, 1_000] },
    ],
    sqlite: [
      { rows: 10_000, payload_bytes: 64, change_counts: [1, 100, 10_000] },
      { rows: 100_000, payload_bytes: 256, change_counts: [1, 100, 10_000] },
      { rows: 1_000_000, payload_bytes: 384, change_counts: [1, 100, 10_000] },
    ],
  }
}

async function runFileScenario(scenario) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "graft-perf-files-"))
  try {
    const fixture = await timed(() => createFiles(root, scenario))
    const opened = await timed(() => RepositorySession.open(root))
    const session = opened.value
    try {
      const init = await timed(() => session.init())
      const stageInitial = await timed(() => session.addAll())
      const initialCommit = await timed(() => session.commit("initial files"))
      const hotCleanStatus = await sample(iterations, () => session.statusIncremental())
      const rounds = []
      for (let round = 0; round < scenario.change_counts.length; round += 1) {
        const changed = Math.min(scenario.change_counts[round], scenario.file_count)
        const paths = filePaths(changed)
        await mutateFiles(root, paths, round)
        const dirtyStatus = await timed(() => session.statusIncremental())
        assert.equal(dirtyStatus.value.status.dirty, true)
        const explicitDiff = await timed(() =>
          session.diffPaths({ paths: paths.slice(0, 100), limit: 100 })
        )
        const stagePaths = await timed(() => session.stagePaths({ paths }))
        const commit = await timed(() => session.commit(`change ${changed} files`))
        const postCommitStatus = await timed(() => session.statusIncremental())
        assert.equal(postCommitStatus.value.status.dirty, false)
        rounds.push({
          changed_paths: changed,
          status_dirty: metric(dirtyStatus),
          explicit_path_diff: metric(explicitDiff),
          stage_paths: metric(stagePaths),
          commit: metric(commit),
          status_after_commit: metric(postCommitStatus),
          status_after_commit_cache_hit:
            postCommitStatus.value.telemetry.status_cache_hit,
        })
      }
      const history = await sample(iterations, () =>
        session.historySummaries({ limit: 50 })
      )
      await session.close()
      const reopened = await timed(() => RepositorySession.open(root))
      const reopenStatus = await timed(() => reopened.value.statusIncremental())
      await reopened.value.close()
      return {
        fixture: scenario,
        fixture_generation: metric(fixture),
        repository_bytes: await directoryBytes(root),
        peak_rss_bytes: peakRssBytes(),
        operations: {
          session_open: metric(opened),
          init: metric(init),
          stage_initial: metric(stageInitial),
          commit_initial: metric(initialCommit),
          clean_status_hot: sampledMetric(hotCleanStatus),
          history_summaries_50: sampledMetric(history),
          reopen: metric(reopened),
          status_after_reopen: metric(reopenStatus),
          persistent_status_hit:
            reopenStatus.value.telemetry.persistent_snapshot_hit,
        },
        mutation_rounds: rounds,
      }
    } finally {
      await session.close().catch(() => undefined)
    }
  } finally {
    await fs.rm(root, { recursive: true, force: true })
  }
}

async function runSqliteScenario(scenario) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "graft-perf-sqlite-"))
  try {
    const databasePath = path.join(root, "space.eidos")
    const fixture = await timed(() => createDatabase(databasePath, scenario))
    const databaseBytes = (await fs.stat(databasePath)).size
    const opened = await timed(() => RepositorySession.open(root))
    const session = opened.value
    try {
      const init = await timed(() => session.init())
      const stageInitial = await timed(() => session.addAll())
      const initialCommit = await timed(() => session.commit("initial database"))
      let priorCommit = commitId(initialCommit.value)
      const hotCleanStatus = await sample(iterations, () => session.statusIncremental())
      const rounds = []
      for (let round = 0; round < scenario.change_counts.length; round += 1) {
        const changed = Math.min(scenario.change_counts[round], scenario.rows)
        await updateRows(databasePath, scenario.rows, changed, round)
        const dirtyStatus = await timed(() => session.statusIncremental())
        assert.equal(dirtyStatus.value.status.dirty, true)
        const workingSummary = await timed(() =>
          session.diffSqlitePaths({ paths: ["space.eidos"], mode: "summary" })
        )
        const workingRows = await timed(() =>
          session.diffSqlitePaths({
            paths: ["space.eidos"],
            mode: "rows",
            table: "records",
            rowLimit: 100,
          })
        )
        const stage = await timed(() =>
          session.stagePaths({ paths: ["space.eidos"] })
        )
        const commit = await timed(() => session.commit(`change ${changed} rows`))
        const nextCommit = commitId(commit.value)
        const postCommitStatus = await timed(() => session.statusIncremental())
        assert.equal(postCommitStatus.value.status.dirty, false)
        const historicalSummary = await timed(() =>
          session.diffSqlitePaths({
            paths: ["space.eidos"],
            from: priorCommit,
            to: nextCommit,
            mode: "summary",
          })
        )
        rounds.push({
          changed_rows: changed,
          status_dirty: metric(dirtyStatus),
          working_summary: metric(workingSummary),
          working_rows_first_100: metric(workingRows),
          stage: metric(stage),
          commit: metric(commit),
          status_after_commit: metric(postCommitStatus),
          status_after_commit_cache_hit:
            postCommitStatus.value.telemetry.status_cache_hit,
          historical_summary: metric(historicalSummary),
        })
        priorCommit = nextCommit
      }
      const history = await sample(iterations, () =>
        session.historySummaries({ limit: 50 })
      )
      await session.close()
      const reopened = await timed(() => RepositorySession.open(root))
      const reopenStatus = await timed(() => reopened.value.statusIncremental())
      await reopened.value.close()
      return {
        fixture: { ...scenario, database_bytes: databaseBytes },
        fixture_generation: metric(fixture),
        repository_bytes: await directoryBytes(root),
        peak_rss_bytes: peakRssBytes(),
        operations: {
          session_open: metric(opened),
          init: metric(init),
          stage_initial: metric(stageInitial),
          commit_initial: metric(initialCommit),
          clean_status_hot: sampledMetric(hotCleanStatus),
          history_summaries_50: sampledMetric(history),
          reopen: metric(reopened),
          status_after_reopen: metric(reopenStatus),
          persistent_status_hit:
            reopenStatus.value.telemetry.persistent_snapshot_hit,
        },
        mutation_rounds: rounds,
      }
    } finally {
      await session.close().catch(() => undefined)
    }
  } finally {
    await fs.rm(root, { recursive: true, force: true })
  }
}

async function createFiles(root, scenario) {
  const pending = []
  for (let index = 0; index < scenario.file_count; index += 1) {
    const file = path.join(root, filePath(index))
    pending.push(
      fs.mkdir(path.dirname(file), { recursive: true }).then(() =>
        fs.writeFile(file, `${index}:`.padEnd(scenario.file_bytes, "x"))
      )
    )
    if (pending.length === 512) {
      await Promise.all(pending)
      pending.length = 0
    }
  }
  await Promise.all(pending)
}

function filePaths(count) {
  return Array.from({ length: count }, (_, index) => filePath(index))
}

function filePath(index) {
  const directory = Math.floor(index / 1_000).toString().padStart(4, "0")
  return path.join("files", directory, `item-${index.toString().padStart(6, "0")}.txt`)
}

async function mutateFiles(root, paths, round) {
  for (let index = 0; index < paths.length; index += 256) {
    await Promise.all(
      paths.slice(index, index + 256).map((relative, offset) =>
        fs.appendFile(path.join(root, relative), `\n${round}:${index + offset}`)
      )
    )
  }
}

function createDatabase(databasePath, scenario) {
  const database = new DatabaseSync(databasePath)
  try {
    database.exec(
      "PRAGMA journal_mode=DELETE; PRAGMA synchronous=OFF; " +
        "CREATE TABLE records (id INTEGER PRIMARY KEY, value TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0)"
    )
    const insert = database.prepare(
      "INSERT INTO records (id, value) VALUES (?, ?)"
    )
    const prefix = "x".repeat(Math.max(1, scenario.payload_bytes - 16))
    database.exec("BEGIN")
    for (let index = 1; index <= scenario.rows; index += 1) {
      insert.run(index, `${prefix}${index.toString(16).padStart(16, "0")}`)
    }
    database.exec("COMMIT; PRAGMA optimize")
  } finally {
    database.close()
  }
}

function updateRows(databasePath, totalRows, count, round) {
  const database = new DatabaseSync(databasePath)
  try {
    const update = database.prepare(
      "UPDATE records SET revision = ?, value = value || ? WHERE id = ?"
    )
    database.exec("BEGIN")
    const stride = Math.max(1, Math.floor(totalRows / count))
    for (let index = 0; index < count; index += 1) {
      update.run(round + 1, `:${round}`, Math.min(totalRows, index * stride + 1))
    }
    database.exec("COMMIT")
  } finally {
    database.close()
  }
}

function commitId(result) {
  const id = result?.commit?.id
  assert.equal(typeof id, "string")
  return id
}

async function runChild(kind, scenario) {
  const childStarted = performance.now()
  const child = spawn(
    process.execPath,
    [benchmarkFile, "--child", kind, JSON.stringify(scenario)],
    { stdio: ["ignore", "pipe", "inherit"] }
  )
  const chunks = []
  child.stdout.on("data", (chunk) => chunks.push(chunk))
  const code = await new Promise((resolve) => child.on("close", resolve))
  if (code !== 0) throw new Error(`${kind} benchmark child exited with ${code}`)
  const result = JSON.parse(Buffer.concat(chunks).toString("utf8"))
  result.scenario_wall_milliseconds = round(performance.now() - childStarted)
  return result
}

async function gitRevision() {
  const child = spawn("git", ["rev-parse", "HEAD"], {
    cwd: path.resolve(import.meta.dirname, "../../.."),
    stdio: ["ignore", "pipe", "ignore"],
  })
  const chunks = []
  child.stdout.on("data", (chunk) => chunks.push(chunk))
  const code = await new Promise((resolve) => child.on("close", resolve))
  return code === 0 ? Buffer.concat(chunks).toString("utf8").trim() : "unknown"
}

async function timed(operation) {
  const started = performance.now()
  const value = await operation()
  return { milliseconds: performance.now() - started, value }
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

function metric(measured) {
  return {
    milliseconds: round(measured.milliseconds),
    response_bytes: jsonBytes(measured.value),
  }
}

function sampledMetric(measured) {
  const sorted = measured.milliseconds.toSorted((left, right) => left - right)
  return {
    samples: measured.milliseconds.map(round),
    min: round(sorted[0]),
    p50: round(percentile(sorted, 0.5)),
    p95: round(percentile(sorted, 0.95)),
    max: round(sorted.at(-1)),
    response_bytes: jsonBytes(measured.values.at(-1)),
  }
}

function percentile(sorted, quantile) {
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * quantile))]
}

function jsonBytes(value) {
  const encoded = JSON.stringify(value)
  return encoded === undefined ? 0 : Buffer.byteLength(encoded)
}

function peakRssBytes() {
  return process.resourceUsage().maxRSS * 1024
}

async function directoryBytes(root) {
  let total = 0
  for (const entry of await fs.readdir(root, { withFileTypes: true })) {
    const target = path.join(root, entry.name)
    total += entry.isDirectory()
      ? await directoryBytes(target)
      : (await fs.stat(target)).size
  }
  return total
}

function positiveInteger(value, fallback) {
  if (value === undefined) return fallback
  const parsed = Number.parseInt(value, 10)
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`expected a positive integer, received ${value}`)
  }
  return parsed
}

function round(value) {
  return Math.round(value * 1_000) / 1_000
}
