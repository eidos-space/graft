# Releasing Graft

Graft has three independent release trains:

- [`.github/workflows/sqlite-extension-release.yml`](.github/workflows/sqlite-extension-release.yml)
  publishes the SQLite extension and `graft` CLI from annotated `vX.Y.Z` tags.
- [`.github/workflows/sdk-release.yml`](.github/workflows/sdk-release.yml) validates pull requests
  and publishes `@eidos.space/graft` from annotated `graft-sdk-vX.Y.Z` tags.
- [`.github/workflows/remote-release.yml`](.github/workflows/remote-release.yml) validates and
  publishes the framework-neutral, Hono, and Cloudflare Remote packages from annotated
  `graft-remote-vX.Y.Z` tags.

All release tag types must point to a commit already merged into `origin/main`. Never release from
a side branch or a dirty checkout.

## Prepare the release commit

Update every workspace crate and the `version` field in `sqlpkg.json` to the same version, then merge that change into `main`. Release tags must point at the current `origin/main` commit; do not release from a side branch.

The latest tag merged into `origin/main` can be found with:

```sh
git fetch --tags origin
git tag --merged origin/main --sort=-v:refname | head -n 1
```

## Validate and publish

From a completely clean checkout of the current `origin/main`, validate the release without changing Git state:

```sh
just run release <VERSION>
```

When those checks pass, create and push the annotated release tag:

```sh
just run release --execute <VERSION>
```

`VERSION` must use `X.Y.Z` or `X.Y.Z-rc.N`. The script rejects dirty worktrees, untracked files, version mismatches, non-main commits, and existing tags.

Pushing the tag starts the release workflow. It builds all CLI and extension targets, packages them, generates `SHA256SUMS`, and only then creates or updates the GitHub release. A version containing a suffix such as `-rc.1` is published as a prerelease.

## Prepare an SDK release

Keep these versions equal:

- `packages/graft-sdk/package.json`
- `crates/graft-sdk/Cargo.toml`
- `crates/graft-sdk-node/Cargo.toml`
- every `@eidos.space/graft-*` optional dependency in the root package

The SDK version is independent from the CLI/SQLite version. Before tagging, merge the SDK pull
request and wait for the full `Release Graft SDK` pull-request matrix. It builds every advertised
binary and tests it on Node.js 20 and 24.

From a clean checkout of the merged `origin/main` commit:

```sh
version=0.2.0
test "$(node -p "require('./packages/graft-sdk/package.json').version")" = "$version"
git tag -a "graft-sdk-v${version}" -m "Graft SDK v${version}"
git push origin "graft-sdk-v${version}"
```

The tag workflow:

1. verifies the annotated tag, `main` ancestry, lockfile, and package/crate versions;
2. builds macOS arm64/x64, Linux glibc arm64/x64, and Windows x64 addons;
3. tests every binary on Node.js 20 and 24;
4. generates one constrained optional npm package per platform and proves the artifact set is
   complete;
5. publishes platform packages before the `@eidos.space/graft` root package;
6. creates a GitHub SDK release containing every addon and `SHA256SUMS`;
7. installs the public root package on all supported platforms and opens a real repository session.

### First npm publish

npm trusted publishing can only be configured after each new package name exists. Bootstrap the
first release with one short-lived granular npm token that can create public packages in the
`@eidos.space` scope. Store it as the `NPM_TOKEN` secret in the GitHub `npm` environment; never put
it in repository files or logs. Delete the secret after the first successful release.

The initial release creates these names:

```text
@eidos.space/graft
@eidos.space/graft-darwin-arm64
@eidos.space/graft-darwin-x64
@eidos.space/graft-linux-arm64-gnu
@eidos.space/graft-linux-x64-gnu
@eidos.space/graft-win32-x64-msvc
```

After the first release, configure each package's npm trusted publisher with:

```text
organization: eidos-space
repository: graft
workflow: sdk-release.yml
environment: npm
allowed action: npm publish
```

The release job has only `contents: write` and `id-token: write`; subsequent publishes use
short-lived npm OIDC credentials and automatic provenance. Configure the GitHub `npm` environment
with required reviewers if release approval is desired. The workflow pins npm 11.16.0 because
trusted publishing requires npm 11.5.1 or newer and Node.js 22.14.0 or newer.

### Partial SDK release recovery

npm versions are immutable and multi-package publication is not atomic. If the workflow stops:

1. do not rebuild or move the tag;
2. inventory the root and all five platform names at the same version;
3. re-run the failed workflow with the unchanged tag and artifacts;
4. the release script skips versions already visible on npm and publishes only missing packages;
5. never publish different bytes under a platform/version that already exists.

New package names can return a successful publish response before their packument is visible from
the registry read path. The release script polls for up to ten minutes after every publish and logs
progress once per minute. Do not retry while that visibility check is active: an early retry can
receive `E403` because the immutable version already exists even though `npm view` still returns
`E404`.

The root package is always published last, after all platform packages are visible. If the root
exists while a platform package is missing, publish that unchanged platform artifact immediately
or deprecate the incomplete root version.

## Prepare a Remote package release

Keep these versions equal:

- `packages/graft-remote/package.json`
- `packages/graft-remote-hono/package.json`
- `packages/graft-remote-cloudflare/package.json`

The Remote release is independent from the CLI/SQLite and resident SDK versions. From a clean
checkout of the merged `origin/main` commit:

```sh
version=0.1.0
node scripts/remote-release.mjs validate "$version"
git tag -a "graft-remote-v${version}" -m "Graft Remote v${version}"
git push origin "graft-remote-v${version}"
```

The workflow builds and tests all three packages, creates deterministic tarballs, verifies their
contents and SHA-256 checksums, publishes core before the Hono and Cloudflare adapters, and then
tests public installs on Node.js 20/24 and in a Wrangler dry-run bundle. It also creates a GitHub
release without replacing the repository's latest CLI/SQLite release.

The first Remote release uses the same short-lived `NPM_TOKEN` bootstrap described above. After
the package names exist, configure each package's npm trusted publisher with:

```text
organization: eidos-space
repository: graft
workflow: remote-release.yml
environment: npm
allowed action: npm publish
```

Then delete `NPM_TOKEN` from the GitHub `npm` environment. If a Remote release stops partway, do
not move or recreate the tag. Re-run the unchanged workflow: it verifies the same release archives,
skips versions already visible on npm, and publishes only missing packages.
