# Releasing Graft

Graft has one release pipeline: [`.github/workflows/sqlite-extension-release.yml`](.github/workflows/sqlite-extension-release.yml). It builds and publishes the SQLite extension and the `graft` CLI for every supported target.

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
