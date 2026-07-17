# Graft Playground

A browser-only Graft playground. The real `graft` CLI is compiled to WebAssembly,
runs in a Web Worker, and stores its repository and SQLite data in OPFS.

The playground uses:

- [`@pierre/trees`](https://trees.software/) for the OPFS explorer
- [`@pierre/diffs`](https://diffs.com/docs) for text and row-diff rendering
- [`@wterm/react`](https://github.com/vercel-labs/wterm/tree/main/packages/%40wterm/react)
  for the command terminal

The IDE-style UI includes an OPFS file tree, editable UTF-8 files, a simple
SQLite table editor, dedicated text and row-diff surfaces, a Bash-like OPFS
shell, staging/history controls, guarded worktree discard actions, and
soft/mixed/hard history reset. Its optional quickstart sidebar runs real commands
in the terminal, downloads a bundled same-origin image into `/attachments/`,
embeds it in Markdown, and remembers task progress in local storage. The file sidebar,
quickstart sidebar, and terminal dock can all be resized.
The interface supports English and Simplified Chinese, follows the browser
language on first use, and remembers the selected language locally.

Use **Reset data** in the header to delete every file for this origin from OPFS
and start again. The confirmation also resets the terminal session and
quickstart progress.

## Run locally

Requirements: pnpm, a Rust toolchain, and a current Emscripten SDK. Emscripten
6.0.3 is the verified version; older SDKs do not understand the Wasm features
emitted by current Rust toolchains.

```bash
pnpm install
EMSDK=/path/to/emsdk pnpm build:wasm
pnpm dev
```

Open the printed localhost URL in a Chromium browser. The repository survives
reloads because `/` is backed by the origin's private file system.

The Vite development and preview servers send COOP/COEP headers required by the
browser runtime. A production host must preserve these headers:

```text
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

## Architecture

```text
React UI ──messages──> Web Worker ──callMain──> graft.wasm
   │                                           │
   └──────── File System Access API ───────────┴──> OPFS /
```

Remote synchronization is intentionally unavailable in this first browser
build. Local repository commands, SQLite snapshots, branches, history, status,
staging, commits, and diffs run entirely on-device.
