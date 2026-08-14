import assert from "node:assert/strict"
import { constants as fsConstants } from "node:fs"
import fs from "node:fs/promises"
import { createRequire } from "node:module"
import os from "node:os"
import path from "node:path"
import { performance } from "node:perf_hooks"

const require = createRequire(import.meta.url)
const { RepositorySession } = require("..")

const fixtureRoot = requiredPath("GRAFT_MERGE_BENCH_FIXTURE_ROOT")
const samples = positiveInteger(process.env.GRAFT_MERGE_BENCH_SAMPLES, 3)
const warmups = positiveInteger(process.env.GRAFT_MERGE_BENCH_WARMUPS, 1, true)
const profile = choice(
  process.env.GRAFT_MERGE_BENCH_PROFILE ?? "ci",
  ["ci", "matrix"],
  "GRAFT_MERGE_BENCH_PROFILE"
)
const temporaryRoot = await fs.mkdtemp(
  path.join(os.tmpdir(), "graft-sync-merge-bench-")
)

const scenarios = [
  {
    name: "text-clean",
    paths: ["docs/Eidos_Sync_BP.md", "business/eidos-sync-go-to-market.md"],
  },
  { name: "text-conflict", paths: ["business/marketing.md"] },
  { name: "sqlite-small", paths: ["dev/eidos-project.eidos"] },
  { name: "sqlite-large", paths: ["dev/perf-test.eidos"] },
  {
    name: "multi-file",
    paths: [
      "dev/eidos-project.eidos",
      "business/users.eidos",
      "docs/Eidos_Sync_BP.md",
      "business/eidos-sync-go-to-market.md",
    ],
  },
  {
    name: "reopen-active-merge",
    paths: ["dev/eidos-project.eidos"],
    reopenAfterApply: true,
  },
  {
    name: "long-divergence",
    paths: ["dev/eidos-project.eidos"],
    extraCommits: 8,
  },
  {
    name: "delete-modify",
    paths: ["business/marketing.md"],
    deleteTheirs: "business/marketing.md",
  },
  {
    name: "portable-line-endings",
    paths: ["business/marketing.md"],
    portableLineEndings: true,
  },
  {
    name: "mixed-large-space",
    paths: [
      "dev/eidos-project.eidos",
      "dev/perf-test.eidos",
      "business/users.eidos",
      "business/marketing.md",
    ],
  },
]
const profileScenarios =
  profile === "ci"
    ? scenarios.filter(({ name }) =>
        ["text-clean", "text-conflict", "sqlite-small", "sqlite-large"].includes(
          name
        )
      )
    : scenarios
const requestedScenarios = new Set(
  (process.env.GRAFT_MERGE_BENCH_SCENARIOS ?? "")
    .split(",")
    .map((name) => name.trim())
    .filter(Boolean)
)
const selectedScenarios = requestedScenarios.size
  ? profileScenarios.filter(({ name }) => requestedScenarios.has(name))
  : profileScenarios
if (requestedScenarios.size && selectedScenarios.length !== requestedScenarios.size) {
  throw new Error("GRAFT_MERGE_BENCH_SCENARIOS contains an unknown or unavailable scenario")
}

try {
  const templates = new Map()
  for (const scenario of selectedScenarios) {
    progress("prepare", scenario.name)
    templates.set(
      scenario.name,
      await prepareScenarioTemplate(scenario, temporaryRoot)
    )
  }

  const measurements = []
  for (const scenario of selectedScenarios) {
    for (let run = -warmups; run < samples; run += 1) {
      progress(run < 0 ? "warmup" : "measure", scenario.name, run)
      const result = await measureScenario(
        templates.get(scenario.name),
        scenario,
        run
      )
      if (run >= 0) measurements.push(result)
    }
  }

  const lifecycleValues = measurements.map(({ lifecycle_ms }) => lifecycle_ms)
  const report = {
    schema: "graft-sync-merge-benchmark-v1",
    generated_at: new Date().toISOString(),
    fixture_root: fixtureRoot,
    profile,
    samples,
    warmups,
    scenarios: selectedScenarios.map(({ name }) => name),
    summary: summarize(lifecycleValues),
    results: selectedScenarios.map(({ name }) => {
      const rows = measurements.filter((measurement) => measurement.scenario === name)
      return {
        scenario: name,
        lifecycle_ms: summarize(rows.map((row) => row.lifecycle_ms)),
        plan_ms: summarize(rows.map((row) => row.plan_ms)),
        apply_ms: summarize(rows.map((row) => row.apply_ms)),
        inspect_ms: summarize(rows.map((row) => row.inspect_ms)),
        resolve_ms: summarize(rows.map((row) => row.resolve_ms)),
        continue_ms: summarize(rows.map((row) => row.continue_ms)),
        reopen_ms: summarize(rows.map((row) => row.reopen_ms)),
        conflicts: summarize(rows.map((row) => row.conflicts)),
      }
    }),
    measurements,
  }
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`)
} finally {
  if (process.env.GRAFT_MERGE_BENCH_KEEP_TEMP === "1") {
    process.stderr.write(`graft-sync-merge-temp ${temporaryRoot}\n`)
  } else {
    await fs.rm(temporaryRoot, { recursive: true, force: true })
  }
}

async function prepareScenarioTemplate(scenario, root) {
  const scenarioRoot = path.join(root, `template-${scenario.name}`)
  const source = path.join(scenarioRoot, "source")
  const remote = path.join(scenarioRoot, "remote")
  const local = path.join(scenarioRoot, "local")
  await fs.mkdir(scenarioRoot)
  await Promise.all([fs.mkdir(source), fs.mkdir(remote), fs.mkdir(local)])
  await copySelected(path.join(fixtureRoot, "base"), source, scenario.paths)
  if (scenario.portableLineEndings) {
    await fs.writeFile(path.join(source, "portable.txt"), "base\n")
    scenario.paths = [...scenario.paths, "portable.txt"]
  }

  const sourceSession = await RepositorySession.open(source)
  const localSession = await RepositorySession.open(local)
  try {
    await sourceSession.init()
    await sourceSession.addAll()
    await sourceSession.commit("benchmark base")
    await sourceSession.configureRemote({
      name: "origin",
      url: `fs://${remote}`,
      upstreamBranch: "main",
    })
    await sourceSession.push()
    await localSession.cloneRepository({ remoteUrl: `fs://${remote}` })

    await copySelected(path.join(fixtureRoot, "theirs"), source, scenario.paths)
    if (scenario.deleteTheirs) await fs.rm(path.join(source, scenario.deleteTheirs))
    if (scenario.portableLineEndings) {
      await fs.writeFile(path.join(source, "portable.txt"), "hosted\r\nline\r\n")
    }
    await sourceSession.addAll()
    if (scenario.deleteTheirs) {
      await sourceSession.untrackPaths({ paths: [scenario.deleteTheirs] })
    }
    await sourceSession.commit("benchmark hosted change")
    for (let index = 0; index < (scenario.extraCommits ?? 0); index += 1) {
      await fs.writeFile(path.join(source, `hosted-${index}.txt`), `hosted ${index}\n`)
      await sourceSession.addAll()
      await sourceSession.commit(`benchmark hosted ${index}`)
    }
    await sourceSession.push()
    await localSession.fetch()

    await copySelected(path.join(fixtureRoot, "ours"), local, scenario.paths)
    if (scenario.portableLineEndings) {
      await fs.writeFile(path.join(local, "portable.txt"), "local\nline\n")
    }
    await localSession.addAll()
    await localSession.commit("benchmark local change")
    for (let index = 0; index < (scenario.extraCommits ?? 0); index += 1) {
      await fs.writeFile(path.join(local, `local-${index}.txt`), `local ${index}\n`)
      await localSession.addAll()
      await localSession.commit(`benchmark local ${index}`)
    }
    await configureEidosMergePolicy(localSession)
  } finally {
    await Promise.all([sourceSession.close(), localSession.close()])
  }
  return local
}

async function measureScenario(template, scenario, run) {
  const sampleRoot = path.join(temporaryRoot, `sample-${scenario.name}-${run}`)
  await cloneDirectory(template, sampleRoot)
  const session = await RepositorySession.open(sampleRoot)
  try {
    const metadata = await session.repositoryMetadata()
    const expectedHead = metadata.current_head
    assert.equal(typeof expectedHead, "string")
    const lifecycleStartedAt = performance.now()

    const planStartedAt = performance.now()
    const plan = await session.planMerge({
      revision: "origin/main",
      expectedHead,
    })
    const planMs = performance.now() - planStartedAt
    assert.equal(plan.kind, "three_way")

    const applyStartedAt = performance.now()
    const applied = await session.applyMerge({
      revision: "origin/main",
      expectedHead,
      planToken: plan.plan_token,
    })
    const applyMs = performance.now() - applyStartedAt
    let merge = applied.merge
    assert.equal(merge.state, "merging")

    let reopenMs = 0
    if (scenario.reopenAfterApply) {
      const reopenStartedAt = performance.now()
      await session.close()
      await session.open()
      merge = await session.getMergeStatus()
      reopenMs = performance.now() - reopenStartedAt
      assert.equal(merge.state, "merging")
    }

    const inspectStartedAt = performance.now()
    const unresolved = await readAllMergePaths(session, merge.state_token, "unmerged")
    let conflicts = 0
    for (const item of unresolved) {
      const page = await session.listMergeConflicts({
        path: item.path,
        limit: 100,
        expectedStateToken: merge.state_token,
      })
      conflicts += page.items.filter(({ status }) => status === "unresolved").length
    }
    const inspectMs = performance.now() - inspectStartedAt

    const resolveStartedAt = performance.now()
    for (const item of unresolved) {
      const resolved = await session.setMergePathResult({
        path: item.path,
        result: "theirs",
        expectedStateToken: merge.state_token,
      })
      merge = resolved.merge
    }
    const resolveMs = performance.now() - resolveStartedAt
    assert.equal(merge.state, "merging")
    assert.equal(merge.unmerged_count, 0)

    const continueStartedAt = performance.now()
    const completed = await session.continueMerge({
      message: "benchmark merge",
      expectedStateToken: merge.state_token,
    })
    const continueMs = performance.now() - continueStartedAt
    assert.equal(completed.merge.state, "none")
    const finalStatus = await session.status()
    assert.equal(
      finalStatus.dirty,
      false,
      `merge left a dirty worktree: ${JSON.stringify(finalStatus)}`
    )

    return {
      scenario: scenario.name,
      run,
      lifecycle_ms: performance.now() - lifecycleStartedAt,
      plan_ms: planMs,
      apply_ms: applyMs,
      inspect_ms: inspectMs,
      resolve_ms: resolveMs,
      continue_ms: continueMs,
      reopen_ms: reopenMs,
      conflicts,
      unresolved_paths: unresolved.length,
    }
  } finally {
    await session.close()
    if (process.env.GRAFT_MERGE_BENCH_KEEP_TEMP !== "1") {
      await fs.rm(sampleRoot, { recursive: true, force: true })
    }
  }
}

async function readAllMergePaths(session, stateToken, filter) {
  const items = []
  let after
  do {
    const page = await session.listMergePaths({
      filter,
      limit: 100,
      ...(after ? { after } : {}),
      expectedStateToken: stateToken,
    })
    items.push(...page.items)
    after = page.next_cursor ?? undefined
  } while (after)
  return items
}

async function configureEidosMergePolicy(session) {
  const current = await session.getMergePolicy()
  const metadataTables = [
    "eidos__features",
    "eidos__fields",
    "eidos__formula_fields",
    "eidos__lookup_fields",
    "eidos__meta",
    "eidos__relation_fields",
    "eidos__tables",
    "eidos__views",
  ]
  const columnResolvers = { ...(current.policy.column_resolvers ?? {}) }
  for (const table of metadataTables) {
    columnResolvers[table] = {
      ...(columnResolvers[table] ?? {}),
      updated_at: "max_timestamp",
    }
  }
  const policy = {
    ...current.policy,
    version: 1,
    same_row_merge: true,
    default_semantic_keys: [
      "_id",
      ...(current.policy.default_semantic_keys ?? []).filter((key) => key !== "_id"),
    ],
    column_resolvers: columnResolvers,
  }
  const validation = await session.validateMergePolicy({ policy })
  assert.equal(validation.valid, true)
  await session.setMergePolicy({
    policy,
    expectedPolicyToken: current.policy_token,
  })
}

async function copySelected(sourceRoot, destinationRoot, relativePaths) {
  for (const relativePath of relativePaths) {
    const source = path.join(sourceRoot, relativePath)
    const destination = path.join(destinationRoot, relativePath)
    try {
      const stat = await fs.lstat(source)
      if (!stat.isFile()) continue
    } catch (error) {
      if (error.code === "ENOENT") continue
      throw error
    }
    await fs.mkdir(path.dirname(destination), { recursive: true })
    await fs.copyFile(source, destination, fsConstants.COPYFILE_FICLONE)
  }
}

async function cloneDirectory(source, destination) {
  await fs.mkdir(destination, { recursive: true })
  const entries = await fs.readdir(source, { withFileTypes: true })
  await Promise.all(
    entries.map(async (entry) => {
      const from = path.join(source, entry.name)
      const to = path.join(destination, entry.name)
      if (entry.isDirectory()) return cloneDirectory(from, to)
      if (entry.isSymbolicLink()) return fs.symlink(await fs.readlink(from), to)
      return fs.copyFile(from, to, fsConstants.COPYFILE_FICLONE)
    })
  )
}

function summarize(values) {
  const sorted = [...values].sort((left, right) => left - right)
  return {
    min: sorted[0] ?? 0,
    median: percentile(sorted, 0.5),
    p95: percentile(sorted, 0.95),
    max: sorted.at(-1) ?? 0,
  }
}

function percentile(sorted, ratio) {
  if (sorted.length === 0) return 0
  return sorted[Math.max(0, Math.ceil(sorted.length * ratio) - 1)]
}

function positiveInteger(value, fallback, allowZero = false) {
  const number = value === undefined ? fallback : Number(value)
  if (!Number.isSafeInteger(number) || number < (allowZero ? 0 : 1)) {
    throw new Error(`expected ${allowZero ? "non-negative" : "positive"} integer, got ${value}`)
  }
  return number
}

function choice(value, choices, name) {
  if (!choices.includes(value)) throw new Error(`${name} must be one of ${choices.join(", ")}`)
  return value
}

function requiredPath(name) {
  const value = process.env[name]?.trim()
  if (!value) throw new Error(`${name} is required`)
  return path.resolve(value)
}

function progress(event, scenario, run) {
  process.stderr.write(
    `graft-sync-merge-progress ${JSON.stringify({ event, scenario, ...(run === undefined ? {} : { run }) })}\n`
  )
}
