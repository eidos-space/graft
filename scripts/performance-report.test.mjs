import assert from "node:assert/strict";
import { test } from "node:test";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  compareMeasurements,
  validateMeasurement,
  readComment,
  releaseDecision,
  normalize,
  createClient,
  resolveRun,
  commentOnRun,
  marker,
  suites,
} from "./performance-report.mjs";

const head = "a".repeat(40),
  priorHead = "b".repeat(40),
  merge = "c".repeat(40),
  previousMerge = "d".repeat(40);
function measurement(value = 100, overrides = {}) {
  return {
    schema: 1,
    headSha: head,
    suite: "cli",
    runner: "ubuntu-24.04",
    architecture: "x64",
    toolchain: "1.91",
    workload: "e".repeat(64),
    samples: 1,
    metrics: { checkout: value },
    ...overrides,
  };
}
function report(measurements = [], major = {}) {
  return {
    schema: 1,
    headSha: measurements[0]?.headSha ?? head,
    measurements,
    baselines: {},
    majorRegression: { cli: false, sdk: false, remote: false, ...major },
  };
}
const body = (data) => marker + "\n<!-- graft-performance-data\n" + JSON.stringify(data) + "\n-->";

test("only a doubling AND 500 ms increase blocks, including zero baselines", () => {
  for (const [before, after, major] of [
    [100, 599, false],
    [100, 600, true],
    [1000, 1501, false],
    [1000, 1999, false],
    [1000, 2000, true],
    [0, 499, false],
    [0, 500, true],
  ]) {
    assert.equal(
      compareMeasurements([measurement(after)], [measurement(before)]).major.length > 0,
      major,
      `${before} -> ${after}`,
    );
  }
});

test("different workloads establish a baseline instead of comparing unlike observations", () => {
  const comparison = compareMeasurements(
    [measurement(9000)],
    [measurement(1, { workload: "f".repeat(64) })],
  );
  assert.equal(comparison.major.length, 0);
  assert.equal(comparison.rows[0].before, null);
});

test("invalid samples, metrics and untrusted comment markup are rejected", () => {
  for (const sample of [
    measurement(100, { samples: 2 }),
    measurement(NaN),
    measurement(-1),
    measurement(10, { metrics: { "bad|<tag>": 1 } }),
  ])
    assert.throws(() => validateMeasurement(sample));
  assert.equal(readComment("untrusted prefix" + body(report())), null);
  assert.equal(readComment(body(report([measurement(), measurement()]))), null);
  assert.equal(readComment(body({ ...report(), headSha: "wrong" })), null);
  assert.deepEqual(readComment(body(report([measurement()]))).measurements, [measurement()]);
});

test("publication requires the exact merge into main and respects each release train", () => {
  const pr = {
    number: 34,
    merged_at: "2026-10-18",
    merge_commit_sha: merge,
    head: { sha: head },
    base: { ref: "main" },
  };
  assert.throws(() => releaseDecision(merge, null, [], "sdk"), /merged/);
  assert.throws(() => releaseDecision(head, pr, [], "sdk"), /merge commit/);
  assert.throws(() => releaseDecision(merge, { ...pr, base: { ref: "other" } }, [], "sdk"), /main/);
  const reports = [report([measurement()], { sdk: true })];
  assert.throws(() => releaseDecision(merge, pr, reports, "sdk"), /major sdk/);
  assert.equal(
    releaseDecision(merge, pr, reports, "remote").performance,
    "recorded-no-major-regression",
  );
  assert.match(releaseDecision(merge, pr, [], "sdk").performance, /unavailable/);
  assert.doesNotThrow(() =>
    releaseDecision(merge, pr, [{ ...reports[0], headSha: priorHead }], "sdk"),
  );
});

test("normalize consumes one real-format observation and refuses multi-round tail data", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "graft-performance-normalize-"));
  try {
    const input = path.join(root, "raw.json"),
      output = path.join(root, "measurement.json");
    const raw = {
      schema_version: 2,
      sample_count: 1,
      provenance: { report_kind: "independent" },
      parameters: { profile: "ci" },
      metrics: [
        {
          name: "checkout",
          unit: "milliseconds",
          lower_is_better: true,
          median: 123,
          samples: [123],
        },
      ],
    };
    await fs.writeFile(input, JSON.stringify(raw));
    const cli = await normalize("cli", input, output, {
      PERF_RUNNER: "ubuntu-24.04",
      PERF_ARCH: "x64",
    });
    assert.equal(cli.metrics.checkout, 123);
    assert.equal(cli.samples, 1);
    raw.sample_count = 6;
    await fs.writeFile(input, JSON.stringify(raw));
    await assert.rejects(
      normalize("cli", input, output, { PERF_RUNNER: "ubuntu-24.04", PERF_ARCH: "x64" }),
    );
    const paired = {
      schema: "graft-e2ee-load-summary-v1",
      rounds: 1,
      order: "plain-first",
      summaries: [
        {
          profile: "encrypted",
          scenario: "objects",
          scale: 100000,
          operation: "initial_push",
          samples: 1,
          milliseconds: { p50: 747, p95: 747 },
        },
      ],
    };
    await fs.writeFile(input, JSON.stringify(paired));
    const core = await normalize("e2ee-core", input, output, {
      PERF_RUNNER: "ubuntu-24.04",
      PERF_ARCH: "x64",
    });
    assert.equal(core.metrics["encrypted/objects/100000/initial_push"], 747);
    paired.summaries[0].milliseconds.p95 = 999;
    await fs.writeFile(input, JSON.stringify(paired));
    await assert.rejects(
      normalize("e2ee-core", input, output, { PERF_RUNNER: "ubuntu-24.04", PERF_ARCH: "x64" }),
    );
    paired.summaries[0].operation = "warm_small_push";
    paired.summaries[0].samples = 3;
    await fs.writeFile(input, JSON.stringify(paired));
    const warm = await normalize("e2ee-core", input, output, {
      PERF_RUNNER: "ubuntu-24.04",
      PERF_ARCH: "x64",
    });
    assert.equal(warm.metrics["encrypted/objects/100000/warm_small_push"], 747);
    await assert.rejects(
      normalize("e2ee-sdk", input, output, { PERF_RUNNER: "ubuntu-24.04", PERF_ARCH: "x64" }),
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

function githubFixture({ releases = true, untrustedMajor = false, maintainerPermission } = {}) {
  const current = {
    number: 34,
    head: { sha: head, repo: { full_name: "eidos-space/graft" } },
    base: { ref: "main" },
  };
  const prior = {
    number: 33,
    merged_at: "2026-10-17",
    merge_commit_sha: previousMerge,
    head: { sha: priorHead },
    base: { ref: "main" },
  };
  const comments = [],
    requests = [];
  const run = {
    id: 71,
    path: ".github/workflows/benchmark.yml",
    name: "Graft Performance",
    event: "pull_request",
    status: "completed",
    head_repository: { id: 123 },
    head_sha: head,
  };
  if (untrustedMajor)
    comments.push({
      user: { login: "stranger" },
      body: body(report([measurement(900)], { sdk: true })),
    });
  if (maintainerPermission)
    comments.push({
      user: { login: "maintainer" },
      author_association: "CONTRIBUTOR",
      body: body(report([measurement(777, { suite: "e2ee-core" })])),
    });
  const client = createClient({
    repository: "eidos-space/graft",
    token: "synthetic-test-token",
    fetcher: async (url, options) => {
      const parsed = new URL(url);
      const route =
        parsed.pathname.slice("/repos/eidos-space/graft".length).replace(/^\//, "") + parsed.search;
      requests.push({ route, method: options.method });
      let result;
      if (options.method === "POST") {
        result = JSON.parse(options.body);
        if (route === "issues/34/comments")
          comments.push({ user: { login: "github-actions[bot]" }, body: result.body });
      } else if (route === "") result = { id: 123 };
      else if (route === "actions/runs/71") result = run;
      else if (route === `commits/${head}/pulls?per_page=100`) result = [current];
      else if (route === `commits/${previousMerge}/pulls?per_page=100`) result = [prior];
      else if (route === "pulls/34") result = current;
      else if (route === "pulls/33") result = prior;
      else if (route === "collaborators/maintainer/permission") {
        if (maintainerPermission === "fail") throw new Error("Permission lookup failed");
        result = { permission: maintainerPermission };
      } else if (route === "collaborators/stranger/permission") result = { permission: "read" };
      else if (route === "issues/34/comments?per_page=100&page=1") result = comments;
      else if (route === "issues/33/comments?per_page=100&page=1")
        result = [
          {
            user: { login: "github-actions[bot]" },
            body: body({
              ...report([measurement(100, { headSha: priorHead })]),
              headSha: priorHead,
            }),
          },
        ];
      else if (route === "releases?per_page=100&page=1")
        result = releases
          ? ["v0.14.0", "graft-sdk-v0.3.32", "graft-remote-v0.2.5"].map((tag_name) => ({
              draft: false,
              tag_name,
              published_at: "2026-10-17",
            }))
          : [];
      else if (route.startsWith("git/ref/tags/"))
        result = { object: { type: "tag", sha: "f".repeat(40) } };
      else if (route === `git/tags/${"f".repeat(40)}`)
        result = { object: { type: "commit", sha: previousMerge } };
      else throw new Error("Unexpected API route " + route);
      return { ok: true, json: async () => JSON.parse(JSON.stringify(result)) };
    },
  });
  return { client, current, run, comments, requests };
}

test("repository metadata uses the GitHub root endpoint without a trailing slash", async () => {
  const urls = [];
  const client = createClient({
    repository: "eidos-space/graft",
    token: "synthetic-test-token",
    fetcher: async (url) => {
      urls.push(url);
      return { ok: true, json: async () => ({ id: 123 }) };
    },
  });
  await client.api("");
  await client.api("actions/runs/71");
  assert.deepEqual(urls, [
    "https://api.github.com/repos/eidos-space/graft",
    "https://api.github.com/repos/eidos-space/graft/actions/runs/71",
  ]);
});

test("recovered reports use verified write access regardless of association labels", async () => {
  for (const permission of ["admin", "maintain", "write", "read", "none"]) {
    const fixture = githubFixture({ maintainerPermission: permission });
    const data = await commentOnRun(fixture.client, 71, [measurement(123, { suite: "e2ee-core" })]);
    assert.equal(
      data.measurements[0].metrics.checkout,
      ["admin", "maintain", "write"].includes(permission) ? 777 : 123,
    );
  }
});

test("a failed author permission lookup cannot discard a recovered report", async () => {
  const fixture = githubFixture({ maintainerPermission: "fail" });
  await assert.rejects(commentOnRun(fixture.client, 71, []), /Permission lookup failed/);
});

test("actual API orchestration reads annotated releases and leaves machine-readable PR evidence", async () => {
  const fixture = githubFixture({ untrustedMajor: true });
  const data = await commentOnRun(fixture.client, 71, [measurement(599)]);
  assert.equal(data.majorRegression.sdk, false);
  assert.equal(data.baselines.sdk, "graft-sdk-v0.3.32");
  assert.equal(data.baselines.cli, "v0.14.0");
  assert.equal(data.baselines.remote, "graft-remote-v0.2.5");
  const parsed = readComment(fixture.comments.at(-1).body);
  assert.equal(parsed.measurements[0].metrics.checkout, 599);
  assert.ok(
    fixture.requests.some((item) => item.route === `statuses/${head}` && item.method === "POST"),
  );
});

test("first observations and major regressions survive faster and unavailable reruns", async () => {
  const fixture = githubFixture();
  const first = await commentOnRun(fixture.client, 71, [measurement(600)]);
  assert.equal(first.majorRegression.sdk, true);
  fixture.run.path = ".github/workflows/e2ee-performance.yml";
  fixture.run.name = "E2EE Performance";
  const second = await commentOnRun(fixture.client, 71, [
    measurement(1),
    measurement(200, { suite: "e2ee-sdk" }),
  ]);
  assert.equal(second.measurements.find((item) => item.suite === "cli").metrics.checkout, 600);
  assert.equal(second.measurements.length, 2);
  assert.equal(second.majorRegression.sdk, true);
  const third = await commentOnRun(fixture.client, 71, []);
  assert.equal(third.majorRegression.sdk, true);
  assert.equal(third.measurements.length, 2);
});

test("a missing baseline and unavailable measurements are recorded without a false pass", async () => {
  const fixture = githubFixture({ releases: false });
  const data = await commentOnRun(fixture.client, 71, []);
  assert.equal(data.baselines.sdk, null);
  assert.equal(data.measurements.length, 0);
  assert.match(fixture.comments.at(-1).body, /缺失测量不是性能通过/);
  assert.ok(suites.every((suite) => fixture.comments.at(-1).body.includes(suite + "=不可用")));
});

test("forks, wrong workflows and obsolete PR heads cannot publish a privileged report", async () => {
  for (const alter of [
    (fixture) => {
      fixture.run.head_repository.id = 999;
    },
    (fixture) => {
      fixture.run.path = ".github/workflows/ci.yml";
    },
    (fixture) => {
      fixture.current.head.repo.full_name = "stranger/graft";
    },
    (fixture) => {
      fixture.current.head.sha = priorHead;
    },
  ]) {
    const fixture = githubFixture();
    alter(fixture);
    assert.equal(await resolveRun(fixture.client, 71), null);
    assert.equal(
      fixture.requests.some((item) => item.method === "POST"),
      false,
    );
  }
});

test("API read failures cannot turn a known regression into an absent report", async () => {
  const client = createClient({
    repository: "eidos-space/graft",
    token: "synthetic",
    fetcher: async () => ({ ok: false, status: 403 }),
  });
  await assert.rejects(client.reports({ number: 34 }), /403/);
});
