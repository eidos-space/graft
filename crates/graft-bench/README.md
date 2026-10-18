# Graft benchmarks

`graft-bench` measures the user-visible repository path end to end. It drives a
release build of the `graft` CLI against a fresh repository for every sample and
emits a versioned JSON report that can be compared across revisions.

## Workload

The `ci` profile uses the same deterministic dataset on every run:

- a 4 KiB-page SQLite database with 20,000 rows and 256-byte seeded,
  high-entropy payloads;
- 64 text files of 4 KiB each;
- two binary files of 2 MiB each;
- a second revision that updates 10% of the rows, one text file, and 64 KiB of
  one binary file; and
- a filesystem remote populated by pushing both repository revisions.

Fixture creation and application mutations happen outside the timed regions.
The measured operations are repository initialization, initial and incremental
staging and commits, row-level diff, checkout/materialization, and a filesystem
remote push. CLI process startup is intentionally included because it is part
of the user-visible latency.

Checkout is measured in a second, identically generated local repository before
any remote synchronization. This keeps the checkout fixture on a branch tip with
both local storage snapshots available; a remote clone currently hydrates the
tip snapshot rather than all historical snapshots. The clone is still validated
byte-for-byte at the pushed tip, then a remote-aware row diff hydrates and
validates the parent snapshot before checking it out and validating it exactly.

Storage metrics include the materialized SQLite database and worktree, total
`.graft` size, incremental history growth, storage amplification, Fjall snapshot
storage, repository objects, external payloads, remaining repository metadata,
file counts, and filesystem-remote breakdowns. Sizes are apparent bytes (the sum
of file lengths), which is more reproducible across filesystems than allocated
block counts.

All generated content uses fixed seeds. Each metric records every fresh-repository
sample plus its median and median absolute deviation. Local paired comparisons
keep base/candidate samples aligned and report per-pair changes and deviation.
PR CI uses the single-observation release policy described below.

## Running locally

Run the CI-sized workload (five samples after one warmup):

```sh
just benchmark
```

Validate the harness quickly with a small dataset:

```sh
just benchmark-smoke
```

Choose the profile, sample count, warmups, and output path explicitly:

```sh
just benchmark ci 7 1 target/benchmark/current.json
```

Compare two reports:

```sh
just benchmark-compare \
  target/benchmark/baseline.json \
  target/benchmark/candidate.json
```

Every reported metric currently uses "lower is better" semantics. For paired
reports, the Markdown comparison only marks a change as an improvement or
regression when its paired median is at least 5% and larger than twice its paired
MAD. Comparing two independently produced reports remains available, but the
output is prominently labeled unpaired and uses marginal medians. Comparisons do
not fail on a regression; performance gates should only be introduced after
enough runner-noise data has been collected.

## GitHub Actions methodology

The PR workflow builds only the candidate and records one CI-sized observation
with `--samples 1 --warmups 0`. It does not rebuild the previous release or run
again for merge and tag pushes. A separate reporter posts the result on the PR
and reads the previous published release's PR comment for comparison. Only
matching runner, architecture, toolchain and workload definitions are compared.
The first observation is retained rather than selecting a faster rerun.

Only a duration at least 2× the baseline and at least 500 ms slower blocks the
corresponding release train. Missing or incompatible baselines are explicitly
reported; they do not produce a performance-pass claim. See `RELEASE.md` for the
publication policy. Single samples cannot establish a P95 or latency SLA.
Raw resources and samples remain available as artifacts. Benchmark jobs have
read-only tokens; the privileged comment workflow never executes PR code.

The local paired comparison commands above remain useful for a specifically
requested investigation. Their six-sample alternating methodology is an
experimental tool, not part of every release.

Changing the workload, metric meaning, or result schema requires incrementing
`REPORT_SCHEMA_VERSION`. Reports with different schemas or dataset parameters
are rejected instead of producing a misleading comparison.

## Interpretation notes

- GitHub-hosted runner timings are noisy. Base/head co-location and medians
  reduce noise, but a small timing change is not proof of a regression.
- The storage snapshot represents the operational on-disk state after each CLI
  process exits. It does not force a Fjall major compaction, because doing so
  would measure an artificial maintenance state rather than normal use.
- Expected local storage components are required to exist and reconcile exactly
  with total `.graft` bytes. Layout changes fail the harness instead of appearing
  as an artificial storage improvement.
- The `smoke` profile is for correctness only; its dataset is too small for
  performance decisions.
- This suite covers the repository/SQLite/remote path. Focused core microbenchmarks
  for page reads, segment frame boundaries, and long-history scaling can be added
  as separate suites without changing this workload.
