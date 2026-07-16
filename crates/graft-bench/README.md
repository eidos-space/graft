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
remote push. CLI process startup and VFS registration are intentionally included
because they are part of the user-visible latency.

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
sample plus its median and median absolute deviation. The CI comparison keeps
base/candidate samples aligned, reports the median per-pair percentage change,
and reports the paired median absolute deviation so shared runner drift is not
mistaken for treatment noise.

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

The benchmark workflow builds the base revision's `graft` executable and the
candidate revision's `graft` executable on the same `ubuntu-24.04` runner. It
then uses the candidate revision's benchmark harness for both executables. This
keeps the dataset and measurement code identical and permits the first benchmark
change to compare against a base revision that did not yet contain this crate.
The six measured samples are paired and alternate `base → candidate` with
`candidate → base`, balancing time-dependent runner drift between revisions.
Paired mode requires an even sample count and writes a shared run ID, harness and
host provenance, and execution order into both reports; the comparator rejects
mismatched pairs.

For pull requests, the base is the PR base SHA. For pushes to `main`, it is the
pre-push SHA (falling back to the first parent when necessary). The comparison
is written to the GitHub job summary, and the raw JSON plus Markdown reports are
uploaded as workflow artifacts. The workflow is read-only and does not execute
through `pull_request_target`, so it is safe for fork pull requests. Superseded
PR runs are cancelled, while every `main` push keeps its own run and report.

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
