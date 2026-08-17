import { constants as fsConstants } from "node:fs"
import fs from "node:fs/promises"
import os from "node:os"
import path from "node:path"
import { performance } from "node:perf_hooks"
import { spawn } from "node:child_process"
import { createRequire } from "node:module"
import { fileURLToPath } from "node:url"
import { DatabaseSync } from "node:sqlite"

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..")
const require = createRequire(import.meta.url)
const { RepositorySession } = require("../packages/graft-sdk")
const allowedEnvironmentKeys = new Set([
  "EIDOS_LITE_STAGING_REMOTE_URL",
  "EIDOS_LITE_STAGING_REMOTE_TOKEN",
  "EIDOS_LITE_STAGING_SPACE_ROOT",
  "GRAFT_STAGING_BENCHMARK_ROUNDS",
  "GRAFT_STAGING_BENCHMARK_KEEP",
])

const environmentPath = process.argv[2]
if (!environmentPath) {
  throw new Error(
    "Usage: node scripts/staging-fetch-pull-benchmark.mjs /owner-only/path.env"
  )
}
const environment = await readOwnerOnlyEnvironment(environmentPath)
const remoteUrl = required(environment, "EIDOS_LITE_STAGING_REMOTE_URL")
const remoteToken = required(environment, "EIDOS_LITE_STAGING_REMOTE_TOKEN")
const sourceRoot = path.resolve(required(environment, "EIDOS_LITE_STAGING_SPACE_ROOT"))
const rounds = parseRounds(environment.GRAFT_STAGING_BENCHMARK_ROUNDS)
const keepBenchmark = environment.GRAFT_STAGING_BENCHMARK_KEEP === "1"
const remote = new URL(remoteUrl)
const localSelfTest = process.env.GRAFT_STAGING_BENCHMARK_ALLOW_LOCAL === "1"
if (
  !localSelfTest &&
  (remote.origin !== "https://sync-staging.eidos.space" || remote.search || remote.hash)
) {
  throw new Error("The benchmark Remote must be an exact sync-staging.eidos.space repository URL")
}
if (localSelfTest && remote.protocol !== "fs:") {
  throw new Error("The local benchmark self-test requires an fs:// Remote")
}

const graft = path.join(repositoryRoot, "target/release/graft")
await fs.access(graft, fsConstants.X_OK)
const sourceMetadata = await fs.stat(sourceRoot)
if (!sourceMetadata.isDirectory()) throw new Error("The benchmark Space root is not a directory")

const benchmarkRoot = await fs.mkdtemp(path.join(os.tmpdir(), "graft-staging-fetch-pull-"))
const publisher = path.join(benchmarkRoot, "publisher")
const receiver = path.join(benchmarkRoot, "receiver")
const childEnvironment = {
  ...process.env,
  EIDOS_LITE_STAGING_REMOTE_TOKEN: remoteToken,
}

try {
  await fs.mkdir(publisher)
  await fs.mkdir(receiver)
  await cloneSpace(sourceRoot, publisher)
  const spaceBytes = await treeBytes(publisher)
  const databases = await findEidosFiles(publisher)
  if (databases.length === 0) throw new Error("The benchmark Space contains no .eidos files")
  const database = await largestFile(databases)
  const remoteWithTokenSelector = new URL(remote)
  if (!localSelfTest) {
    remoteWithTokenSelector.searchParams.set("token_env", "EIDOS_LITE_STAGING_REMOTE_TOKEN")
  }

  await runGraft(["init", "--json"], publisher)
  await runGraft(["add", "--all", "--json"], publisher)
  await runGraft(["commit", "--message", "Staging fetch/pull benchmark base", "--json"], publisher)
  await runGraft(
    ["remote", "add", "origin", remoteWithTokenSelector.toString(), "--json"],
    publisher
  )
  await runGraft(["push", "origin", "main", "--json"], publisher, 30 * 60_000)
  await runGraft(["clone", remoteWithTokenSelector.toString(), "main", "--json"], receiver, 30 * 60_000)

  const receiverSession = await RepositorySession.open(receiver)
  receiverSession.setHttpBearerToken("origin", remoteToken)
  const results = []
  try {
    for (let round = 1; round <= rounds; round += 1) {
      appendBenchmarkRow(database, round)
      await runGraft(["add", "--all", "--json"], publisher)
      const commit = await runGraft(
        ["commit", "--message", `Staging fetch/pull benchmark ${round}`, "--json"],
        publisher
      )
      await runGraft(["push", "origin", "main", "--json"], publisher, 30 * 60_000)

      const before = (await receiverSession.repositoryMetadata()).current_head
      if (typeof before !== "string") throw new Error("Receiver has no current head")
      const fetch = await timedSdk((onProgress) =>
        receiverSession.fetch({ remote: "origin", branch: "main", onProgress })
      )
      const plan = await timedSdk(() =>
        receiverSession.planMerge({ revision: "origin/main", expectedHead: before })
      )
      if (plan.value.kind !== "fast_forward") {
        throw new Error(`Expected fast_forward, received ${String(plan.value.kind)}`)
      }
      const apply = await timedSdk((onProgress) =>
        receiverSession.applyMerge({
          revision: "origin/main",
          expectedHead: before,
          planToken: plan.value.plan_token,
          onProgress,
        })
      )
      const after = (await receiverSession.repositoryMetadata()).current_head
      const commitValue = JSON.parse(commit.stdout)
      const expected = commitValue.current_head ?? commitValue.head ?? commitValue.commit?.id
      if (typeof expected === "string" && after !== expected) {
        throw new Error("Fast-forward Pull did not materialize the published head")
      }

      results.push({
        round,
        fetch_ms: fetch.durationMs,
        fetch_transferred_bytes: fetch.transfer.transferredBytes,
        fetch_total_bytes: fetch.transfer.totalBytes,
        plan_ms: plan.durationMs,
        pull_ms: apply.durationMs,
        pull_transferred_bytes: apply.transfer.transferredBytes,
        pull_total_bytes: apply.transfer.totalBytes,
        sync_ms: fetch.durationMs + plan.durationMs + apply.durationMs,
      })
    }
  } finally {
    await receiverSession.close()
  }

  const summary = {
    schema: "graft-staging-fetch-pull-benchmark-v1",
    space_bytes: spaceBytes,
    database_bytes: (await fs.stat(database)).size,
    rounds,
    results,
    median: {
      fetch_ms: median(results.map((result) => result.fetch_ms)),
      plan_ms: median(results.map((result) => result.plan_ms)),
      pull_ms: median(results.map((result) => result.pull_ms)),
      sync_ms: median(results.map((result) => result.sync_ms)),
    },
  }
  process.stdout.write(`${JSON.stringify(summary, null, 2)}\n`)
} finally {
  if (keepBenchmark) {
    process.stderr.write(`Benchmark files retained at ${benchmarkRoot}\n`)
  } else {
    await fs.rm(benchmarkRoot, { recursive: true, force: true })
  }
}

async function readOwnerOnlyEnvironment(file) {
  const metadata = await fs.stat(file)
  if (!metadata.isFile() || (metadata.mode & 0o077) !== 0) {
    throw new Error("The staging benchmark environment file must be a regular owner-only file")
  }
  const values = {}
  for (const rawLine of (await fs.readFile(file, "utf8")).split(/\r?\n/u)) {
    const line = rawLine.trim()
    if (!line || line.startsWith("#")) continue
    const separator = line.indexOf("=")
    if (separator <= 0) throw new Error("Invalid staging benchmark environment line")
    const key = line.slice(0, separator).trim()
    if (!allowedEnvironmentKeys.has(key) || Object.hasOwn(values, key)) {
      throw new Error(`Unsupported or duplicate staging benchmark key: ${key}`)
    }
    values[key] = line.slice(separator + 1).trim()
  }
  return values
}

function required(values, key) {
  const value = values[key]
  if (!value) throw new Error(`Missing ${key}`)
  return value
}

function parseRounds(value) {
  if (value === undefined) return 3
  const rounds = Number(value)
  if (!Number.isSafeInteger(rounds) || rounds < 1 || rounds > 10) {
    throw new Error("GRAFT_STAGING_BENCHMARK_ROUNDS must be between 1 and 10")
  }
  return rounds
}

async function cloneSpace(source, destination, relative = "") {
  const directory = path.join(source, relative)
  for await (const entry of await fs.opendir(directory)) {
    if (!relative && entry.name === ".graft") continue
    const nextRelative = path.join(relative, entry.name)
    const from = path.join(source, nextRelative)
    const to = path.join(destination, nextRelative)
    if (entry.isDirectory()) {
      await fs.mkdir(to)
      await cloneSpace(source, destination, nextRelative)
    } else if (entry.isFile()) {
      await fs.copyFile(from, to, fsConstants.COPYFILE_FICLONE)
    } else {
      throw new Error(`Unsupported Space entry type: ${nextRelative}`)
    }
  }
}

async function findEidosFiles(root, relative = "", files = []) {
  for await (const entry of await fs.opendir(path.join(root, relative))) {
    if (entry.name === ".graft") continue
    const nextRelative = path.join(relative, entry.name)
    if (entry.isDirectory()) await findEidosFiles(root, nextRelative, files)
    else if (entry.isFile() && entry.name.toLowerCase().endsWith(".eidos")) {
      files.push(path.join(root, nextRelative))
    }
  }
  return files
}

async function largestFile(files) {
  const sizes = await Promise.all(files.map(async (file) => [(await fs.stat(file)).size, file]))
  sizes.sort((left, right) => right[0] - left[0])
  return sizes[0][1]
}

function appendBenchmarkRow(databasePath, round) {
  const database = new DatabaseSync(databasePath)
  try {
    database.exec(
      "CREATE TABLE IF NOT EXISTS graft_staging_benchmark (id INTEGER PRIMARY KEY, round INTEGER NOT NULL, value TEXT NOT NULL)"
    )
    database
      .prepare("INSERT INTO graft_staging_benchmark(round, value) VALUES (?, ?)")
      .run(round, `round-${round}-${Date.now()}`)
  } finally {
    database.close()
  }
}

async function runGraft(arguments_, cwd, timeoutMs = 5 * 60_000) {
  return timedGraft(arguments_, cwd, timeoutMs)
}

async function timedGraft(arguments_, cwd, timeoutMs = 5 * 60_000) {
  const started = performance.now()
  const { stdout } = await spawnCapture(graft, arguments_, cwd, timeoutMs)
  return {
    stdout,
    durationMs: roundMilliseconds(performance.now() - started),
  }
}

async function timedSdk(operation) {
  let transferredBytes = 0
  let totalBytes
  const onProgress = (progress) => {
    if (Number.isFinite(progress?.transferredBytes)) {
      transferredBytes = Math.max(transferredBytes, progress.transferredBytes)
    }
    if (Number.isFinite(progress?.totalBytes)) totalBytes = progress.totalBytes
  }
  const started = performance.now()
  const value = await operation(onProgress)
  return {
    value,
    durationMs: roundMilliseconds(performance.now() - started),
    transfer: { transferredBytes, totalBytes: totalBytes ?? null },
  }
}

async function spawnCapture(command, arguments_, cwd, timeoutMs) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, arguments_, {
      cwd,
      env: childEnvironment,
      stdio: ["ignore", "pipe", "pipe"],
    })
    let stdout = ""
    let stderr = ""
    const timeout = setTimeout(() => child.kill("SIGTERM"), timeoutMs)
    child.stdout.setEncoding("utf8")
    child.stderr.setEncoding("utf8")
    child.stdout.on("data", (chunk) => (stdout += chunk))
    child.stderr.on("data", (chunk) => (stderr += chunk))
    child.once("error", (error) => {
      clearTimeout(timeout)
      reject(error)
    })
    child.once("exit", (code, signal) => {
      clearTimeout(timeout)
      if (code === 0) resolve({ stdout, stderr })
      else reject(new Error(`graft ${arguments_[0]} failed (${code ?? signal}): ${safeError(stderr)}`))
    })
  })
}

function safeError(stderr) {
  return stderr
    .split(remoteToken)
    .join("[redacted]")
    .slice(-2_000)
}

async function treeBytes(root) {
  let bytes = 0
  for await (const entry of await fs.opendir(root)) {
    const child = path.join(root, entry.name)
    if (entry.isDirectory()) bytes += await treeBytes(child)
    else if (entry.isFile()) bytes += (await fs.stat(child)).size
  }
  return bytes
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right)
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2 === 0
    ? roundMilliseconds((sorted[middle - 1] + sorted[middle]) / 2)
    : sorted[middle]
}

function roundMilliseconds(value) {
  return Math.round(value * 1_000) / 1_000
}
