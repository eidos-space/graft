import { spawnSync } from "node:child_process"
import fs from "node:fs/promises"
import os from "node:os"
import path from "node:path"
import { performance } from "node:perf_hooks"
import { createRequire } from "node:module"

const require = createRequire(import.meta.url)
const { RepositorySession } = require("..")

const iterations = positiveInteger(
  process.env.GRAFT_SDK_BENCH_ITERATIONS,
  30
)
const workspace = path.resolve(import.meta.dirname, "../../..")
const graftBinary = path.resolve(
  process.env.GRAFT_CLI_PATH ??
    path.join(workspace, "target", "release", "graft")
)
const temporaryRoot = await fs.mkdtemp(
  path.join(os.tmpdir(), "graft-sdk-bench-")
)

try {
  const repository = path.join(temporaryRoot, "repository")
  await fs.mkdir(repository)
  const setupSession = await RepositorySession.open(repository)
  await setupSession.init()
  await fs.writeFile(path.join(repository, "note.txt"), "benchmark\n")
  await setupSession.addAll()
  await setupSession.commit("benchmark baseline")
  await setupSession.close()

  assertCliAvailable(graftBinary, repository)

  const sdkCold = await sample(iterations, async () => {
    const coldSession = await RepositorySession.open(repository)
    await coldSession.status()
    await coldSession.close()
  })
  const cliStatusCold = sampleSync(iterations, () =>
    runCli(graftBinary, repository, ["status", "--json"])
  )
  const cliDiffCold = sampleSync(iterations, () =>
    runCli(graftBinary, repository, ["diff", "--json"])
  )

  const hotSession = await RepositorySession.open(repository)
  const sdkStatusHot = await sample(iterations, () => hotSession.status())
  const sdkDiffHot = await sample(iterations, () => hotSession.diff())
  await hotSession.close()

  const report = {
    schema: "graft-sdk-benchmark-v1",
    generated_at: new Date().toISOString(),
    environment: {
      platform: process.platform,
      arch: process.arch,
      node: process.version,
      iterations,
      graft_binary: graftBinary,
    },
    milliseconds: {
      sdk_open_status_close_cold: summarize(sdkCold),
      sdk_status_hot: summarize(sdkStatusHot),
      sdk_diff_hot: summarize(sdkDiffHot),
      cli_status_process_cold: summarize(cliStatusCold),
      cli_diff_process_cold: summarize(cliDiffCold),
    },
  }
  report.speedup = {
    status_p50:
      report.milliseconds.cli_status_process_cold.p50 /
      report.milliseconds.sdk_status_hot.p50,
    diff_p50:
      report.milliseconds.cli_diff_process_cold.p50 /
      report.milliseconds.sdk_diff_hot.p50,
  }
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
} finally {
  await fs.rm(temporaryRoot, { force: true, recursive: true })
}

async function sample(count, operation) {
  const values = []
  for (let index = 0; index < count; index += 1) {
    const started = performance.now()
    await operation()
    values.push(performance.now() - started)
  }
  return values
}

function sampleSync(count, operation) {
  const values = []
  for (let index = 0; index < count; index += 1) {
    const started = performance.now()
    operation()
    values.push(performance.now() - started)
  }
  return values
}

function runCli(binary, cwd, args) {
  const result = spawnSync(binary, args, {
    cwd,
    encoding: "utf8",
    env: { ...process.env, NO_COLOR: "1" },
  })
  if (result.status !== 0) {
    throw new Error(
      `CLI benchmark command failed (${args.join(" ")}): ${
        result.stderr || result.error?.message || result.status
      }`
    )
  }
  JSON.parse(result.stdout)
}

function assertCliAvailable(binary, cwd) {
  runCli(binary, cwd, ["status", "--json"])
}

function summarize(values) {
  const sorted = values.toSorted((left, right) => left - right)
  return {
    first: round(values[0]),
    min: round(sorted[0]),
    p50: round(percentile(sorted, 0.5)),
    p95: round(percentile(sorted, 0.95)),
    max: round(sorted.at(-1)),
    mean: round(
      values.reduce((total, value) => total + value, 0) / values.length
    ),
  }
}

function percentile(sorted, quantile) {
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * quantile))]
}

function positiveInteger(value, fallback) {
  if (value === undefined) return fallback
  const parsed = Number.parseInt(value, 10)
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error("GRAFT_SDK_BENCH_ITERATIONS must be a positive integer")
  }
  return parsed
}

function round(value) {
  return Math.round(value * 1000) / 1000
}
