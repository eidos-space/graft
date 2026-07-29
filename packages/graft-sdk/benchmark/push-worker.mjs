import { spawnSync } from "node:child_process"
import { createRequire } from "node:module"
import { DatabaseSync } from "node:sqlite"
import fs from "node:fs/promises"
import os from "node:os"
import path from "node:path"
import { performance } from "node:perf_hooks"

const require = createRequire(import.meta.url)
const { RepositorySession } = require("..")

const mode = requiredChoice("GRAFT_PUSH_BENCH_MODE", [
  "cli-cold",
  "sdk-cold",
  "sdk-warm",
])
const target = requiredChoice("GRAFT_PUSH_BENCH_TARGET", ["fs", "http"])
const iterations = positiveInteger(process.env.GRAFT_PUSH_BENCH_ITERATIONS, 10)
const largeBytes = positiveInteger(
  process.env.GRAFT_PUSH_BENCH_LARGE_BYTES,
  256 * 1024
)
const workspace = path.resolve(import.meta.dirname, "../../..")
const graftBinary = path.resolve(
  process.env.GRAFT_CLI_PATH ?? path.join(workspace, "target", "release", "graft")
)
const temporaryRoot = await fs.mkdtemp(path.join(os.tmpdir(), "graft-push-bench-"))
const repository = path.join(temporaryRoot, "repository")
const localRemote = path.join(temporaryRoot, "remote")
const branch = benchmarkBranch(mode)
const remoteUrl =
  target === "fs"
    ? `fs://${localRemote}`
    : requiredEnvironment("GRAFT_PUSH_BENCH_HTTP_REMOTE")
const token = process.env.GRAFT_REMOTE_TOKEN
let warmSession

try {
  await fs.mkdir(repository)
  if (target === "fs") await fs.mkdir(localRemote)

  await initialize()
  await fs.writeFile(path.join(repository, "note.txt"), "baseline\n")
  createDatabase(path.join(repository, "data.eidos"))
  await addAndCommit("benchmark baseline")
  runCli(["branch", "--json", "--move", branch])
  await configureRemote()
  if (mode === "sdk-warm") {
    warmSession = await RepositorySession.open(repository)
    setToken(warmSession)
  }

  const samples = []
  samples.push(await measuredPush("first", 0))
  for (let run = 0; run < iterations; run += 1) {
    samples.push(await measuredPush("noop", run))
  }
  for (let run = 0; run < iterations; run += 1) {
    await fs.appendFile(path.join(repository, "note.txt"), `line ${run}\n`)
    await addAndCommit(`text ${run}`)
    samples.push(await measuredPush("text", run))
  }
  for (let run = 0; run < iterations; run += 1) {
    insertRow(path.join(repository, "data.eidos"), run + 2)
    await addAndCommit(`row ${run}`)
    samples.push(await measuredPush("sqlite_row", run))
  }
  for (let run = 0; run < iterations; run += 1) {
    await fs.writeFile(
      path.join(repository, "representative.bin"),
      deterministicBytes(largeBytes, run)
    )
    await addAndCommit(`representative ${run}`)
    samples.push(await measuredPush("representative", run))
  }

  await closeWarmSession()
  await deleteRemoteBranch()
  process.stdout.write(
    JSON.stringify({ mode, target, iterations, large_bytes: largeBytes, samples }) + "\n"
  )
} finally {
  await closeWarmSession()
  await fs.rm(temporaryRoot, { recursive: true, force: true })
}

async function initialize() {
  if (mode === "cli-cold") {
    runCli(["init", "--json"])
    return
  }
  await withSession((session) => session.init())
}

async function configureRemote() {
  if (mode === "cli-cold") {
    runCli(["remote", "add", "--json", "origin", remoteUrl])
    runCli(["branch", "--json", "--set-upstream-to", `origin/${branch}`])
    return
  }
  await withSession((session) =>
    session.configureRemote({
      name: "origin",
      url: remoteUrl,
      ...(token ? { bearerToken: token } : {}),
      upstreamBranch: branch,
    })
  )
}

async function addAndCommit(message) {
  if (mode === "cli-cold") {
    runCli(["add", "--json", "--all"])
    runCli(["commit", "--json", "--message", message])
    return
  }
  await withSession(async (session) => {
    await session.addAll()
    await session.commit(message)
  })
}

async function measuredPush(caseName, run) {
  marker("start", caseName, run)
  const started = performance.now()
  try {
    if (mode === "cli-cold") {
      const result = runCli(["push", "--json"])
      if (result.stderr) process.stderr.write(result.stderr)
    } else {
      await withSession((session) => session.push())
    }
    return { case: caseName, run, milliseconds: performance.now() - started }
  } finally {
    marker("end", caseName, run)
  }
}

async function deleteRemoteBranch() {
  try {
    if (mode === "cli-cold") {
      runCli(["push", "--json", "origin", `:${branch}`])
      return
    }
    runCli(["push", "--json", "origin", `:${branch}`])
  } catch {
    process.stderr.write(
      'graft-bench-marker {"event":"cleanup_failed","case":"cleanup","run":0}\n'
    )
  }
}

async function withSession(operation) {
  if (warmSession !== undefined) return await operation(warmSession)
  const session = await RepositorySession.open(repository)
  setToken(session)
  try {
    return await operation(session)
  } finally {
    await session.close()
  }
}

function setToken(session) {
  if (token) session.setHttpBearerToken("origin", token)
}

async function closeWarmSession() {
  if (warmSession === undefined) return
  const session = warmSession
  warmSession = undefined
  await session.close()
}

function runCli(args) {
  const result = spawnSync(graftBinary, args, {
    cwd: repository,
    encoding: "utf8",
    env: { ...process.env, NO_COLOR: "1" },
    maxBuffer: 32 * 1024 * 1024,
    timeout: 120_000,
  })
  if (result.status !== 0) {
    throw new Error(`CLI ${args[0]} failed with status ${result.status ?? "unknown"}`)
  }
  if (result.stdout.trim()) JSON.parse(result.stdout)
  return result
}

function createDatabase(databasePath) {
  const database = new DatabaseSync(databasePath)
  database.exec("CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
  database.prepare("INSERT INTO items(id, value) VALUES (?, ?)").run(1, "baseline")
  database.close()
}

function insertRow(databasePath, id) {
  const database = new DatabaseSync(databasePath)
  database.prepare("INSERT INTO items(id, value) VALUES (?, ?)").run(id, `value-${id}`)
  database.close()
}

function deterministicBytes(length, seed) {
  const bytes = Buffer.allocUnsafe(length)
  for (let index = 0; index < length; index += 1) {
    bytes[index] = (index * 31 + seed * 17) & 0xff
  }
  return bytes
}

function marker(event, caseName, run) {
  process.stderr.write(
    `graft-bench-marker ${JSON.stringify({ event, case: caseName, run })}\n`
  )
}

function benchmarkBranch(value) {
  const suffix = Math.random().toString(36).slice(2, 10)
  return `perf-${Date.now().toString(36)}-${value.replaceAll("-", "")}-${suffix}`
}

function requiredEnvironment(name) {
  const value = process.env[name]?.trim()
  if (!value) throw new Error(`${name} is required`)
  return value
}

function requiredChoice(name, choices) {
  const value = requiredEnvironment(name)
  if (!choices.includes(value)) throw new Error(`${name} is invalid`)
  return value
}

function positiveInteger(value, fallback) {
  if (value === undefined) return fallback
  const parsed = Number.parseInt(value, 10)
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error("Benchmark numeric options must be positive integers")
  }
  return parsed
}
