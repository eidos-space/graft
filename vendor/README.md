# Vendored dependencies

This directory contains third-party Rust crates that Graft patches locally.
The root `Cargo.toml` lists them as explicit workspace members so normal
workspace checks cover them, and uses them through explicit path dependencies.
Unmodified dependencies should continue to come from crates.io.

Keep the upstream version in each crate's `Cargo.toml` unchanged so Cargo can
detect incompatible dependency updates. When replacing a snapshot, start from
the matching crates.io release, reapply only the patches documented below, and
run `cargo fmt --check`, `cargo check`, and the relevant native and WebAssembly
tests.

## `fjall` 3.1.6

- Upstream: <https://github.com/fjall-rs/fjall>
- Upstream revision recorded by the crate: `80cf6bcce931a9f65dac3d0558abd02564107630`
- Local patch locations: `src/file.rs` and `src/locked_file.rs`

Graft runs Fjall on Emscripten's OPFS backend. That environment cannot fsync
directory handles and does not provide the process-level file locking used by
native Fjall. The local patch makes directory fsync a no-op and skips lock and
unlock operations only when `target_os = "emscripten"`. Native targets retain
the upstream behavior. Other source differences from the published crate are
formatting-only. Skipping locks assumes one owner for an OPFS database; do not
open the same database concurrently from multiple browser workers or tabs.

Remove this vendored copy once an upstream Fjall release supports this
Emscripten behavior and Graft has upgraded to it.

## `sqlite-plugin` 0.9.0

- Upstream: <https://github.com/orbitinghail/sqlite-plugin>
- Upstream revision recorded by the crate: `eb4caa9d6d592344cf4affc7c57143a1a8cf2078`
- Local patch locations: `build.rs` and `src/vfs.rs`

The local patches address two issues:

1. SQLite's `sqlite3_mprintf` treats its first argument as a format string.
   Passing PRAGMA output directly corrupts literal percent sequences and can
   read nonexistent variadic arguments. The patch copies output through the
   constant `"%s"` format and includes a regression test with Unicode and `%`.
2. Bindgen omits several static SQLite entry points for the Emscripten target.
   The build script appends the required declarations; the symbols are supplied
   by `libsqlite3-sys` at link time.

The whitespace-only change in `sqlite3/sqlite3ext.h` is not a behavioral patch.
Remove this vendored copy once upstream releases contain both functional fixes
and Graft has upgraded to that release.
