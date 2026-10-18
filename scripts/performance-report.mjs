import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const marker = "<!-- graft-performance:v1 -->";
export const surfaces = {
  cli: /^v\d+\.\d+\.\d+(?:-[\w.-]+)?$/,
  sdk: /^graft-sdk-v\d+\.\d+\.\d+(?:-[\w.-]+)?$/,
  remote: /^graft-remote-v\d+\.\d+\.\d+(?:-[\w.-]+)?$/,
};
export const suites = ["cli", "e2ee-core", "e2ee-sdk", "e2ee-hydration"];
export const policy = { ratio: 2, increaseMs: 500 };
const sha = /^[a-f0-9]{40}$/;
const metricName = /^[A-Za-z0-9_./:-]{1,200}$/;

export function validateMeasurement(value) {
  assert.equal(value.schema, 1);
  assert.match(value.headSha, sha);
  assert.ok(suites.includes(value.suite));
  assert.equal(value.runner, "ubuntu-24.04");
  assert.equal(value.architecture, "x64");
  assert.equal(value.toolchain, "1.91");
  assert.match(value.workload, /^[a-f0-9]{64}$/);
  assert.equal(value.samples, 1);
  assert.ok(
    value.metrics &&
      Object.keys(value.metrics).length > 0 &&
      Object.keys(value.metrics).length <= 200,
  );
  const metrics = {};
  for (const [name, milliseconds] of Object.entries(value.metrics)) {
    assert.match(name, metricName);
    assert.ok(Number.isFinite(milliseconds) && milliseconds >= 0 && milliseconds < 86_400_000);
    metrics[name] = milliseconds;
  }
  return {
    schema: 1,
    headSha: value.headSha,
    suite: value.suite,
    runner: value.runner,
    architecture: value.architecture,
    toolchain: value.toolchain,
    workload: value.workload,
    samples: 1,
    metrics,
  };
}

export function compareMeasurements(current, previous = []) {
  const rows = [],
    major = [];
  for (const raw of current) {
    const item = validateMeasurement(raw);
    const before = previous
      .map(validateMeasurement)
      .find((old) =>
        ["suite", "runner", "architecture", "toolchain", "workload"].every(
          (key) => old[key] === item[key],
        ),
      );
    for (const [name, value] of Object.entries(item.metrics)) {
      const baseline = before?.metrics[name];
      const delta = baseline === undefined ? null : value - baseline;
      const regression =
        baseline !== undefined && value >= baseline * policy.ratio && delta >= policy.increaseMs;
      const row = {
        suite: item.suite,
        metric: name,
        value,
        before: baseline ?? null,
        delta,
        regression,
      };
      rows.push(row);
      if (regression) major.push(row);
    }
  }
  return { rows, major };
}

export function readComment(body) {
  if (typeof body !== "string" || !body.startsWith(marker) || body.length > 64_000) return null;
  try {
    const data = JSON.parse(/<!-- graft-performance-data\n([^]*?)\n-->/.exec(body)?.[1] ?? "null");
    assert.equal(data.schema, 1);
    assert.match(data.headSha, sha);
    assert.ok(Array.isArray(data.measurements) && data.measurements.length <= suites.length);
    data.measurements = data.measurements.map(validateMeasurement);
    assert.equal(
      new Set(data.measurements.map((item) => item.suite)).size,
      data.measurements.length,
    );
    assert.ok(data.measurements.every((item) => item.headSha === data.headSha));
    for (const surface of Object.keys(surfaces))
      assert.equal(typeof data.majorRegression[surface], "boolean");
    return data;
  } catch {
    return null;
  }
}

export function releaseDecision(commit, pr, reports, surface) {
  assert.match(commit ?? "", sha);
  assert.ok(Object.hasOwn(surfaces, surface), "Unknown release surface");
  assert.ok(pr?.merged_at && pr.base.ref === "main", "Release requires a PR merged into main");
  assert.equal(pr.merge_commit_sha, commit, "Release tag must point to the PR merge commit");
  assert.ok(
    !reports.some((data) => data.headSha === pr.head.sha && data.majorRegression[surface]),
    `PR #${pr.number} records a major ${surface} performance regression`,
  );
  return {
    pr: pr.number,
    surface,
    commit,
    performance: reports.some((data) => data.headSha === pr.head.sha && data.measurements.length)
      ? "recorded-no-major-regression"
      : "unavailable-not-a-performance-pass",
  };
}

export async function normalize(suite, input, output, environment = process.env) {
  assert.ok(suites.includes(suite));
  const report = JSON.parse(await fs.readFile(input, "utf8"));
  const metrics = {};
  let workload;
  if (suite === "cli") {
    assert.equal(report.schema_version, 2);
    assert.equal(report.sample_count, 1);
    assert.equal(report.provenance.report_kind, "independent");
    for (const metric of report.metrics) {
      if (metric.unit !== "milliseconds" || metric.lower_is_better !== true) continue;
      assert.equal(metric.samples.length, 1);
      assert.equal(metric.median, metric.samples[0]);
      assert.ok(!Object.hasOwn(metrics, metric.name), "Duplicate metric");
      metrics[metric.name] = metric.median;
    }
    workload = report.parameters;
  } else {
    assert.equal(report.schema, "graft-e2ee-load-summary-v1");
    assert.equal(report.rounds, 1);
    assert.equal(report.order, "plain-first");
    for (const row of report.summaries) {
      assert.equal(row.samples, 1);
      const name = [row.profile, row.scenario, row.scale, row.operation].join("/");
      assert.ok(!Object.hasOwn(metrics, name), "Duplicate operation");
      // One observation is not a tail estimate. Keep raw P50/P95 and resources
      // in the artifact, and compare the single elapsed-time observation here.
      assert.equal(row.milliseconds.p50, row.milliseconds.p95);
      metrics[name] = row.milliseconds.p50;
    }
    workload = { order: report.order, cases: Object.keys(metrics).sort() };
  }
  const sources =
    suite === "cli"
      ? ["crates/graft-bench/src/model.rs", "crates/graft-bench/src/runner.rs"]
      : [
          "scripts/e2ee-load-benchmark.py",
          {
            "e2ee-core": "crates/graft/examples/e2ee_load_benchmark.rs",
            "e2ee-sdk": "crates/graft-sdk/examples/e2ee_sync_benchmark.rs",
            "e2ee-hydration": "crates/graft-sdk/examples/e2ee_sqlite_hydration_benchmark.rs",
          }[suite],
        ];
  const fingerprint = createHash("sha256").update(JSON.stringify(workload));
  for (const file of sources) fingerprint.update(await fs.readFile(file));
  const value = validateMeasurement({
    schema: 1,
    headSha: execFileSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).trim(),
    suite,
    runner: environment.PERF_RUNNER,
    architecture: environment.PERF_ARCH,
    toolchain: "1.91",
    workload: fingerprint.digest("hex"),
    samples: 1,
    metrics,
  });
  await fs.writeFile(output, JSON.stringify(value) + "\n");
  return value;
}

export function createClient({
  repository,
  token,
  fetcher = fetch,
  origin = "https://api.github.com",
}) {
  assert.match(repository ?? "", /^[\w.-]+\/[\w.-]+$/);
  assert.ok(token, "Missing GitHub credential");
  const api = async (route, method = "GET", body) => {
    const response = await fetcher(`${origin}/repos/${repository}${route ? `/${route}` : ""}`, {
      method,
      headers: {
        Authorization: `Bearer ${token}`,
        Accept: "application/vnd.github+json",
        "Content-Type": "application/json",
        "X-GitHub-Api-Version": "2022-11-28",
      },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(30_000),
    });
    if (!response.ok) throw new Error(`GitHub ${method} ${route}: ${response.status}`);
    return response.json();
  };
  const comments = async (number) => {
    const result = [];
    for (let page = 1; page <= 10; page++) {
      const items = await api(`issues/${number}/comments?per_page=100&page=${page}`);
      result.push(...items);
      if (items.length < 100) return result;
    }
    throw new Error("PR comment pagination exceeded; cannot discard older regression records");
  };
  const reports = async (pr) =>
    (await comments(pr.number))
      .filter(
        (item) =>
          item.user.login === "github-actions[bot]" || item.user.login === repository.split("/")[0],
      )
      .map((item) => readComment(item.body))
      .filter((data) => data?.headSha === pr.head.sha);
  const associatedPr = async (commit) => {
    assert.match(commit, sha);
    const items = await api(`commits/${commit}/pulls?per_page=100`);
    const item = items.find((pr) => pr.merged_at && pr.merge_commit_sha === commit);
    return item ? api(`pulls/${item.number}`) : null;
  };
  return { repository, api, reports, associatedPr };
}

export async function resolveRun(client, runId) {
  assert.match(String(runId), /^\d+$/);
  const run = await client.api(`actions/runs/${runId}`);
  const repo = await client.api("");
  const workflows = {
    ".github/workflows/benchmark.yml": "Graft Performance",
    ".github/workflows/e2ee-performance.yml": "E2EE Performance",
  };
  if (
    workflows[run.path] !== run.name ||
    !["pull_request", "workflow_dispatch"].includes(run.event) ||
    run.status !== "completed" ||
    run.head_repository?.id !== repo.id
  )
    return null;
  assert.match(run.head_sha, sha);
  const items = await client.api(`commits/${run.head_sha}/pulls?per_page=100`);
  const item = items.find(
    (pr) =>
      pr.head.sha === run.head_sha &&
      pr.head.repo?.full_name === client.repository &&
      pr.base.ref === "main",
  );
  if (!item) return null;
  const pr = await client.api(`pulls/${item.number}`);
  if (pr.head.sha !== run.head_sha || pr.head.repo?.full_name !== client.repository) return null;
  return { run, pr };
}

async function previousReports(client, surface, releases) {
  const release = releases
    .filter((item) => !item.draft && surfaces[surface].test(item.tag_name))
    .sort((a, b) => Date.parse(b.published_at) - Date.parse(a.published_at))[0];
  if (!release) return { tag: null, measurements: [] };
  let object = (await client.api(`git/ref/tags/${release.tag_name}`)).object;
  for (let depth = 0; object.type === "tag" && depth < 4; depth++)
    object = (await client.api(`git/tags/${object.sha}`)).object;
  assert.equal(object.type, "commit");
  const pr = await client.associatedPr(object.sha);
  const measurements = new Map();
  if (pr)
    for (const data of await client.reports(pr))
      for (const item of data.measurements)
        if (!measurements.has(item.suite)) measurements.set(item.suite, item);
  return { tag: release.tag_name, measurements: [...measurements.values()] };
}

export function renderComment(data, comparisons) {
  const lines = [
    marker,
    "### Graft 性能记录",
    "",
    `提交：\`${data.headSha}\` · 每组单次测量；单个观测不表示 P95 或 SLA。`,
    "",
    "| 发布线 | 上次发布 | 可比指标 | 重大退化 |",
    "| --- | --- | ---: | ---: |",
  ];
  for (const [surface, comparison] of Object.entries(comparisons)) {
    lines.push(
      `| ${surface} | ${data.baselines[surface] ?? "无基线"} | ${comparison.rows.filter((row) => row.before !== null).length} | ${data.majorRegression[surface] ? "阻塞" : "无已记录重大退化"} |`,
    );
    for (const row of comparison.major.slice(0, 12))
      lines.push(
        `- ${surface}: \`${row.suite}/${row.metric}\`：${row.before.toFixed(1)} → ${row.value.toFixed(1)} ms`,
      );
    if (comparison.major.length > 12)
      lines.push(`另有 ${comparison.major.length - 12} 个重大退化指标，详见完整计时数据。`);
  }
  lines.push(
    "",
    "每组展示增加最多的一项（无基线时展示耗时最长的一项）；完整计时数据保留在下方。",
    "",
    "| 发布线 | 测量组 / 操作 | 本次 | 上次发布 | 增加 |",
    "| --- | --- | ---: | ---: | ---: |",
  );
  for (const [surface, comparison] of Object.entries(comparisons)) {
    for (const suite of suites) {
      const row = comparison.rows
        .filter((item) => item.suite === suite)
        .sort((a, b) => (b.delta ?? b.value) - (a.delta ?? a.value))[0];
      if (row)
        lines.push(
          `| ${surface} | ${suite} / ${row.metric} | ${row.value.toFixed(1)} ms | ${row.before === null ? "—" : row.before.toFixed(1) + " ms"} | ${row.delta === null ? "—" : row.delta.toFixed(1) + " ms"} |`,
        );
    }
  }
  const available = new Set(data.measurements.map((item) => item.suite));
  lines.push(
    "",
    `测量组：${suites.map((suite) => `${suite}=${available.has(suite) ? "已留底" : "不可用"}`).join("；")}。`,
    "",
    "只比较相同 runner、架构、Rust 工具链和负载定义。耗时至少翻倍且增加至少 500 ms 才阻塞对应发布线；绝对预算超出只留报告。资源指标和旧预算结果保留在原始 artifact 中。缺失测量不是性能通过，也不能清除同提交已知的重大退化。",
    "",
    ...data.runUrls.map((url, index) => `[原始 CI 观测 ${index + 1}](${url})`),
    "",
    "<details><summary>下次发布使用的数据</summary>",
    "",
    "<!-- graft-performance-data",
    JSON.stringify(data),
    "-->",
    "</details>",
  );
  const body = lines.join("\n");
  assert.ok(body.length < 64_000, "Performance comment too large");
  return body;
}

export async function commentOnRun(client, runId, measurements) {
  const resolved = await resolveRun(client, runId);
  if (!resolved) return null;
  const { run, pr } = resolved;
  const existing = await client.reports(pr);
  const retained = new Map();
  // Preserve the first observation for each suite, never select a faster rerun.
  for (const data of existing)
    for (const item of data.measurements)
      if (!retained.has(item.suite)) retained.set(item.suite, item);
  for (const raw of measurements) {
    const item = validateMeasurement(raw);
    assert.equal(item.headSha, pr.head.sha, "Stale or unrelated measurement");
    if (!retained.has(item.suite)) retained.set(item.suite, item);
  }
  const data = {
    schema: 1,
    headSha: pr.head.sha,
    measurements: [...retained.values()],
    baselines: {},
    majorRegression: {},
    runUrl: `https://github.com/${client.repository}/actions/runs/${run.id}`,
  };
  data.runUrls = [
    ...new Set([...existing.flatMap((old) => old.runUrls ?? [old.runUrl]), data.runUrl]),
  ]
    .filter(
      (url) =>
        typeof url === "string" &&
        url.startsWith(`https://github.com/${client.repository}/actions/runs/`) &&
        /\/\d+$/.test(url),
    )
    .slice(-20);
  const releases = [];
  for (let page = 1; page <= 10; page++) {
    const batch = await client.api(`releases?per_page=100&page=${page}`);
    releases.push(...batch);
    if (batch.length < 100) break;
    assert.ok(page < 10, "Release pagination exceeded");
  }
  const comparisons = {};
  for (const surface of Object.keys(surfaces)) {
    const previous = await previousReports(client, surface, releases);
    comparisons[surface] = compareMeasurements(data.measurements, previous.measurements);
    data.baselines[surface] = previous.tag;
    data.majorRegression[surface] =
      comparisons[surface].major.length > 0 || existing.some((old) => old.majorRegression[surface]);
  }
  // Recheck after reading baselines: never publish a status for an obsolete head.
  if ((await client.api(`pulls/${pr.number}`)).head.sha !== data.headSha) return null;
  await client.api(`issues/${pr.number}/comments`, "POST", {
    body: renderComment(data, comparisons),
  });
  const major = Object.values(data.majorRegression).some(Boolean);
  await client.api(`statuses/${pr.head.sha}`, "POST", {
    state: major ? "failure" : "success",
    context: "Graft performance regression",
    description: major
      ? "Major regression recorded; see release-train comparison"
      : data.measurements.length
        ? "Single-run report retained; no known major regression"
        : "Measurement unavailable; not a performance pass",
    target_url: data.runUrl,
  });
  return data;
}

async function main() {
  const mode = process.argv[2];
  if (mode === "normalize") return normalize(...process.argv.slice(3, 6));
  const client = createClient({
    repository: process.env.GITHUB_REPOSITORY,
    token: process.env.GH_TOKEN,
  });
  if (mode === "guard") {
    const commit = execFileSync("git", ["rev-parse", "HEAD"], { encoding: "utf8" }).trim();
    const pr = await client.associatedPr(commit);
    // Fail on API errors: treating a failed read as an empty report could bypass
    // a known major regression. A genuinely absent report is non-blocking.
    console.log(
      JSON.stringify(
        releaseDecision(
          commit,
          pr,
          pr ? await client.reports(pr) : [],
          process.env.GRAFT_RELEASE_SURFACE,
        ),
      ),
    );
    return;
  }
  const runId = process.env.PERF_RUN_ID;
  if (mode === "resolve") {
    const resolved = await resolveRun(client, runId);
    await fs.appendFile(process.env.GITHUB_OUTPUT, `eligible=${resolved !== null}\n`);
    return;
  }
  assert.equal(mode, "comment");
  const measurements = [];
  const entries = await fs.readdir(process.env.PERF_REPORT_DIR, {
    recursive: true,
    withFileTypes: true,
  });
  for (const entry of entries.filter((item) => item.isFile() && item.name === "measurement.json")) {
    const file = path.join(entry.parentPath, entry.name);
    assert.ok((await fs.stat(file)).size < 48_000);
    measurements.push(validateMeasurement(JSON.parse(await fs.readFile(file, "utf8"))));
  }
  assert.equal(
    new Set(measurements.map((item) => item.suite)).size,
    measurements.length,
    "Duplicate suite artifact",
  );
  console.log(JSON.stringify(await commentOnRun(client, runId, measurements)));
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url))
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
