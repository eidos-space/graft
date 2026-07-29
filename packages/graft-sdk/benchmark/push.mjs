import { spawnSync } from "node:child_process"
import path from "node:path"

const worker = path.join(import.meta.dirname, "push-worker.mjs")
const iterations = positiveInteger(process.env.GRAFT_PUSH_BENCH_ITERATIONS, 10)
const targets = selectedChoices(
  "GRAFT_PUSH_BENCH_TARGETS",
  ["fs", "http"],
  process.env.GRAFT_PUSH_BENCH_HTTP_REMOTE ? ["fs", "http"] : ["fs"]
)
const modes = selectedChoices(
  "GRAFT_PUSH_BENCH_MODES",
  ["cli-cold", "sdk-cold", "sdk-warm"],
  ["cli-cold", "sdk-cold", "sdk-warm"]
)
const runs = []

for (const target of targets) {
  for (const mode of modes) {
    progress("start", target, mode)
    const modeRemote =
      process.env[
        `GRAFT_PUSH_BENCH_HTTP_REMOTE_${mode.replaceAll("-", "_").toUpperCase()}`
      ] ?? process.env.GRAFT_PUSH_BENCH_HTTP_REMOTE
    const result = spawnSync(process.execPath, [worker], {
      encoding: "utf8",
      env: {
        ...process.env,
        GRAFT_PUSH_BENCH_MODE: mode,
        GRAFT_PUSH_BENCH_TARGET: target,
        ...(target === "http"
          ? { GRAFT_PUSH_BENCH_HTTP_REMOTE: modeRemote }
          : {}),
        GRAFT_PUSH_BENCH_ITERATIONS: String(iterations),
        GRAFT_PUSH_TRACE: "1",
        NO_COLOR: "1",
      },
      maxBuffer: 64 * 1024 * 1024,
      timeout: 30 * 60 * 1000,
    })
    if (result.status !== 0) {
      const lastTrace = lastSafeTrace(result.stderr)
      throw new Error(
        `Push benchmark worker failed for ${target}/${mode} with status ${
          result.status ?? "unknown"
        }${lastTrace === undefined ? "" : ` after ${JSON.stringify(lastTrace)}`}`
      )
    }
    const report = JSON.parse(result.stdout)
    const traces = parseTraces(result.stderr)
    for (const sample of report.samples) {
      const key = sampleKey(sample.case, sample.run)
      runs.push({
        target,
        mode,
        ...sample,
        ...(traces.get(key) ?? emptyTrace()),
      })
    }
    progress("complete", target, mode)
  }
}

function lastSafeTrace(stderr) {
  let last
  let lastHttp
  let lastSample
  for (const line of stderr.split(/\r?\n/)) {
    if (line.startsWith("graft-bench-marker ")) {
      const marker = JSON.parse(line.slice("graft-bench-marker ".length))
      if (marker.event === "start") {
        lastSample = { case: marker.case, run: marker.run }
      }
      continue
    }
    if (!line.startsWith("graft-push-trace ")) continue
    const event = JSON.parse(line.slice("graft-push-trace ".length))
    const trace = {
      event: event.event,
      ...(event.operation ? { operation: event.operation } : {}),
      ...(event.phase ? { phase: event.phase } : {}),
      ...(event.status !== undefined ? { status: event.status } : {}),
      ...(event.duration_ms !== undefined
        ? { duration_ms: event.duration_ms }
        : {}),
    }
    last = trace
    if (event.event === "http_request") lastHttp = trace
  }
  const trace = lastHttp ?? last
  return trace === undefined ? lastSample : { ...lastSample, ...trace }
}

function progress(event, target, mode) {
  process.stderr.write(
    `graft-bench-progress ${JSON.stringify({ event, target, mode })}\n`
  )
}

const groups = new Map()
for (const run of runs) {
  const key = `${run.target}/${run.mode}/${run.case}`
  const group = groups.get(key) ?? []
  group.push(run)
  groups.set(key, group)
}

const results = [...groups.entries()].map(([key, samples]) => {
  const [target, mode, caseName] = key.split("/")
  return {
    target,
    mode,
    case: caseName,
    runs: samples.length,
    milliseconds: summarize(samples.map((sample) => sample.milliseconds)),
    requests: summarize(samples.map((sample) => sample.requests)),
    request_bytes: summarize(samples.map((sample) => sample.request_bytes)),
    response_bytes: summarize(samples.map((sample) => sample.response_bytes)),
    http_client_ms: summarize(samples.map((sample) => sample.http_client_ms)),
    server_total_ms: summarize(samples.map((sample) => sample.server_total_ms)),
    server_auth_ms: summarize(samples.map((sample) => sample.server_auth_ms)),
    server_directory_ms: summarize(
      samples.map((sample) => sample.server_directory_ms)
    ),
  }
})

process.stdout.write(
  JSON.stringify(
    {
      schema: "graft-push-benchmark-v1",
      generated_at: new Date().toISOString(),
      environment: {
        platform: process.platform,
        arch: process.arch,
        node: process.version,
        iterations,
        http_enabled: targets.includes("http"),
      },
      results,
    },
    null,
    2
  ) + "\n"
)

function parseTraces(stderr) {
  const traces = new Map()
  let active
  for (const line of stderr.split(/\r?\n/)) {
    if (line.startsWith("graft-bench-marker ")) {
      const marker = JSON.parse(line.slice("graft-bench-marker ".length))
      if (marker.event === "start") {
        active = sampleKey(marker.case, marker.run)
        traces.set(active, emptyTrace())
      } else if (marker.event === "end") {
        active = undefined
      }
      continue
    }
    if (active === undefined || !line.startsWith("graft-push-trace ")) continue
    const event = JSON.parse(line.slice("graft-push-trace ".length))
    if (event.event !== "http_request") continue
    const trace = traces.get(active)
    trace.requests += 1
    trace.request_bytes += event.request_bytes ?? 0
    trace.response_bytes += event.response_bytes ?? 0
    trace.http_client_ms += event.duration_ms ?? 0
    trace.server_total_ms += event.server_timing_ms?.total ?? 0
    trace.server_auth_ms += event.server_timing_ms?.auth ?? 0
    trace.server_directory_ms += event.server_timing_ms?.directory ?? 0
  }
  return traces
}

function emptyTrace() {
  return {
    requests: 0,
    request_bytes: 0,
    response_bytes: 0,
    http_client_ms: 0,
    server_total_ms: 0,
    server_auth_ms: 0,
    server_directory_ms: 0,
  }
}

function sampleKey(caseName, run) {
  return `${caseName}/${run}`
}

function summarize(values) {
  const sorted = values.toSorted((left, right) => left - right)
  return {
    min: round(sorted[0]),
    p50: round(percentile(sorted, 0.5)),
    p95: round(percentile(sorted, 0.95)),
    max: round(sorted.at(-1)),
    mean: round(values.reduce((total, value) => total + value, 0) / values.length),
  }
}

function percentile(sorted, quantile) {
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * quantile))]
}

function round(value) {
  return Math.round(value * 1_000) / 1_000
}

function positiveInteger(value, fallback) {
  if (value === undefined) return fallback
  const parsed = Number.parseInt(value, 10)
  if (!Number.isInteger(parsed) || parsed <= 0) {
    throw new Error("GRAFT_PUSH_BENCH_ITERATIONS must be a positive integer")
  }
  return parsed
}

function selectedChoices(name, choices, fallback) {
  const value = process.env[name]?.trim()
  if (!value) return fallback
  const selected = [...new Set(value.split(",").map((item) => item.trim()))]
  if (
    selected.length === 0 ||
    selected.some((item) => !choices.includes(item))
  ) {
    throw new Error(`${name} must contain only: ${choices.join(", ")}`)
  }
  return selected
}
