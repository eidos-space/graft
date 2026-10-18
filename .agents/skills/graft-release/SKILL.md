---
name: graft-release
description: Prepare and verify Graft CLI, Node SDK and Remote releases through PRs and official CI, including versions, release notes, single-run performance comments and public artifact checks. Use for Graft release preparation or publication; publishing Graft does not publish Eidos Lite.
---

# Graft release

Read the repository `AGENTS.md` and `RELEASE.md`. Preserve unrelated dirty
changes. Updating code does not authorize commits, PRs, tags or publication;
follow the user's existing authorization and the repository Git policy.

Choose the release train before changing versions:

| Train | Versions and notes | Annotated tag / publisher |
| --- | --- | --- |
| CLI/core | Six crates listed in `RELEASE.md`, root `CHANGELOG.md` | `vX.Y.Z` / `cli-release.yml` |
| Node SDK | SDK Rust crates, npm root and five generated native packages, SDK `CHANGELOG.md` | `graft-sdk-vX.Y.Z` / `sdk-release.yml` |
| Remote | Three Remote packages and their changelog | `graft-remote-vX.Y.Z` / `remote-release.yml` |

Put the version, dependency updates and concrete release notes in a PR to
`main`. Run focused checks for the changed behavior and reuse the PR's full
functional/platform results. Keep security, compatibility and data-preservation
checks required. Do not rebuild the same native packages locally and in several
candidate workflows for CI-only changes.

Performance is independent of release builds. `benchmark.yml` measures the CLI
once; `e2ee-performance.yml` runs one round each for core, SDK and hydration.
`performance-report.yml` comments results and links raw evidence on the PR.
Read that comment before publishing. Each train compares with its latest
published release's PR data, using the same runner, architecture, Rust toolchain
and workload fingerprint. Only an elapsed-time regression of at least 2× **and**
at least +500 ms blocks that train. Absolute-budget overruns are retained for
reference, not used to delay publication.

Preserve the first valid observation per suite and commit. Do not rerun to get
a better number, clear a known regression with missing evidence, or copy a
measurement to a different commit without proof. No compatible baseline means
establish one; missing measurements mean unavailable, not a performance pass.
Fix an actual major regression in a new commit and measure once. A failed API
read can be retried without rerunning successful builds. Multi-round paired
experiments remain available for an explicitly scoped diagnosis.

After the PR is merged and the user has authorized publication, use its exact
merge commit, verify a clean checkout and absent tag, then create one annotated
tag. The publisher validates PR association and rechecks recorded regressions
before publication. Use official CI and npm OIDC; do not publish npm packages
locally or move a tag whose artifacts may have reached consumers. For a partial
release, rerun only failed work against the same immutable tag and package
versions, following `RELEASE.md` recovery guidance.

Verify the remote tag/source, exact workflow outcome, curated notes, platform
asset set, checksums and actual public install/API behavior. Report built,
published and installed separately, with URLs, PR performance evidence and any
explicitly deferred platform coverage. When Eidos consumes a new SDK, report
the formal version and supported native profiles so its separate release can
pin the published dependency.
