# AGENTS.md

This file is the working guide for coding agents in this repository. Keep it
aligned with the current tree, CI workflows, and `CONTRIBUTING.md`.

## Product model and current boundaries

Graft provides Git-like version control for application state stored in
ordinary SQLite database files and app-owned files. The supported integration
surfaces are the `graft` CLI and its JSON output, the Node.js/Electron SDK, and
the HTTP remote packages.

- The Rust package is named `graft-cli`; its executable is still named `graft`.
- A custom SQLite VFS and loadable SQLite extension are no longer part of the
  repository. Do not reintroduce extension registration, VFS file handles, or
  PRAGMA-based command dispatch as compatibility shims.
- `graft-sqlite` remains the internal SQLite adapter for consistent physical
  database snapshots, row-level diff and merge, and worktree materialization.
- Repository commands flow through the typed repository command service. The
  CLI does not open a control database to dispatch commands.
- Storage layouts, object serialization, and private Rust module boundaries
  are implementation details unless a specification explicitly makes them an
  observable contract.

## Repository map

| Path | Responsibility |
| --- | --- |
| `crates/graft` | Core storage engine, repository model, refs, index, merge, sync, and remote backends. |
| `crates/graft-sqlite` | Physical SQLite snapshot/import, row diff/merge, and worktree support. |
| `crates/graft-cli` | `graft` command-line executable and CLI integration tests. |
| `crates/graft-sdk` | Long-lived embedded Rust repository sessions. |
| `crates/graft-sdk-node` | Node-API bindings for the resident SDK. |
| `crates/graft-test` | Shared Rust test support and integration tests. |
| `crates/graft-bench` | Reproducible CLI performance and storage benchmarks. |
| `crates/graft-tracing` | Shared tracing setup. |
| `packages/graft-sdk` | Published Node.js/Electron package, tests, and release tooling. |
| `packages/graft-remote*` | Framework-neutral, Hono, and Cloudflare remote packages. |
| `services/graft-remote-cloudflare` | Deployable Cloudflare Worker used to verify the remote stack. |
| `web-demo` | Browser/WASM playground. |
| `docs` | Astro/Starlight documentation and the implementation-aligned specifications. |
| `vendor/fjall` | Pinned vendored storage dependency; avoid changing it incidentally. |

Generated native binaries, npm release directories, benchmark artifacts, and
documentation build output should not be edited by hand.

## Toolchain and setup

- Rust MSRV: 1.91. The workspace uses Cargo resolver 3 and Rust 2024 crates.
- Install `just` and `cargo-nextest` for the standard Rust workflow.
- Use pnpm 10. Node.js 24 is the recommended contributor runtime; the published
  SDK supports Node.js 20 and newer.
- The root JavaScript workspace, `docs`, and `web-demo` have separate lockfiles.
  Install dependencies from the directory whose package is being changed.

```bash
# Root remote packages and service
pnpm install --frozen-lockfile

# Documentation
pnpm --dir docs install --frozen-lockfile

# Browser playground
pnpm --dir web-demo install --frozen-lockfile
```

## Build, run, and test

### Rust workspace

```bash
cargo build --workspace
cargo check --workspace --all-targets
cargo fmt --all --check
cargo clippy --workspace --all-targets --exclude fjall --no-deps
cargo nextest run
cargo test --doc
```

`just test` runs the Rust nextest suite and Rust doctests; it does not run the
JavaScript, documentation, or browser suites. `just build-all` builds the Rust
workspace.

Prefer the smallest relevant test while iterating, then widen verification for
the affected surface:

```bash
cargo nextest run -p graft-cli
cargo nextest run -p graft-sqlite
cargo nextest run -p graft <test-filter>

cargo run -p graft-cli -- --version
cargo run -p graft-cli -- <command> [arguments...]
just run tool vid   # also accepts log or sid
```

### Node.js/Electron SDK

```bash
pnpm --dir packages/graft-sdk build:native
pnpm --dir packages/graft-sdk test
```

When SDK code or packaging changes, keep the versions in `graft-sdk`,
`graft-sdk-node`, the npm package, platform packages, and `Cargo.lock`
synchronized. Do not edit generated `.node` binaries directly.

### Remote packages and Cloudflare verification service

```bash
pnpm check:remote
pnpm test:remote
pnpm build:remote
```

### Documentation

```bash
pnpm --dir docs build
cmp install.sh docs/dist/install.sh
sh -n docs/dist/install.sh
```

Update the English and Chinese page together when both versions exist. The
English specification files under `docs/specs` are normative; Chinese files
are section-aligned references. Some index language may still refer to the
removed VFS/extension profile—treat that as documentation debt, not as a reason
to restore removed code.

### Browser/WASM playground

```bash
pnpm --dir web-demo build:wasm   # needed after relevant Rust/WASM changes
pnpm --dir web-demo build
pnpm --dir web-demo test:e2e
```

The web demo exposes only capabilities implemented by its WASM host. Do not use
mocks as evidence that a native-only capability works in the browser.

### Benchmarks

```bash
just benchmark-smoke
just benchmark ci 5 1 target/benchmark/current.json
just benchmark-compare <baseline.json> <candidate.json>
```

Do not overwrite checked-in benchmark baselines unless the task explicitly
requires a new baseline and the environment is comparable.

## Architecture and behavior invariants

- Repository paths are normalized UTF-8 paths relative to the worktree and
  must never address `.graft` internals.
- Ordinary SQLite connections write physical database files. `graft add`
  captures a consistent private snapshot, including committed WAL frames,
  without requiring a manual checkpoint.
- Operations that replace checked-out databases must respect SQLite writer
  locks and normalize or remove WAL sidecars safely. Tests should cover active
  writer rejection when changing this path.
- Read-only planning and inspection must not move refs, replace worktree files,
  resolve conflicts, or silently hydrate unrelated state.
- Refs are mutable pointers; repository objects and snapshots are immutable and
  content-derived. Never mutate an existing object in place.
- Remote credentials are explicit adapter inputs. Do not persist tokens in
  repository config, remote URLs, caches, logs, or result payloads.
- Preserve structured errors and JSON output contracts at CLI/SDK boundaries.
  Avoid parsing human-readable CLI output in integrations.

Consult `docs/specs/README.md` to find the normative owner of observable
repository, storage, diff, merge, sync, adapter, and worktree behavior. Update
the owning specification and executable evidence when changing a contract.

## Searching and editing

- For a named type, function, module, or file, find the matching file first:
  `rg --files | rg '<name>'`.
- Use `rg -n '<exact-pattern>' <paths>` for exact definitions and call sites.
  Use semantic search only after the likely owner and exact names are known.
- Exclude `target`, `node_modules`, generated documentation output, native
  binaries, and checked-in benchmark artifacts from broad searches.
- Keep changes scoped. Preserve unrelated user changes and do not rewrite
  vendored code, lockfiles, generated bindings, or release artifacts unless the
  task requires them.

## Coding guidelines

Follow `CONTRIBUTING.md` and the workspace lint configuration.

- Prioritize safety, predictable performance, and clarity.
- Prefer simple explicit control flow, focused functions, and types that encode
  units and invariants.
- Avoid recursion, unnecessary dynamic dispatch, hidden side effects, and
  unchecked arithmetic at storage boundaries.
- Use assertions for internal invariants and structured errors for recoverable
  input, I/O, concurrency, and compatibility failures.
- Keep batching and I/O costs visible. Network work dominates disk, which
  dominates memory and CPU; avoid per-item round trips.
- Add or update focused tests with every behavior change. For regressions, make
  the test demonstrate the failure before relying on the fix.
- Run `cargo fmt` after Rust edits and keep the first-party clippy invocation
  aligned with CI. Vendored `fjall` is compiled and tested but excluded from
  first-party clippy policy.

## Git, release, and collaboration safety

- Never commit, push, tag, publish, deploy, or open a pull request unless the
  user explicitly asks for that action.
- Work may occur in a dirty or shared worktree. Inspect `git status` before and
  after editing, preserve unrelated changes, and do not reset or clean them.
- Do not use destructive Git commands such as `git reset --hard`, forced branch
  updates, or force-push without explicit authorization and an exact target.
- Pull requests target `main`. Run checks proportional to the changed surfaces
  before handing work back.
- Core/CLI releases use a `vX.Y.Z` tag and `just run release X.Y.Z`; the
  `--execute` flag creates and pushes the tag, so use it only when explicitly
  authorized. SDK and remote packages have separate release workflows.
