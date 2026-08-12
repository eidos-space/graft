#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
package_dir="$(cd "${script_dir}/.." && pwd)"
workspace_dir="$(cd "${package_dir}/../.." && pwd)"
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
rust_target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
output_dir="${2:-${package_dir}/native}"

case "${rust_target}" in
  aarch64-apple-darwin)
    cargo_library="${target_dir}/${rust_target}/release/libgraft_sdk_node.dylib"
    package_library="${output_dir}/graft-sdk.darwin-arm64.node"
    ;;
  x86_64-apple-darwin)
    cargo_library="${target_dir}/${rust_target}/release/libgraft_sdk_node.dylib"
    package_library="${output_dir}/graft-sdk.darwin-x64.node"
    ;;
  aarch64-unknown-linux-gnu)
    cargo_library="${target_dir}/${rust_target}/release/libgraft_sdk_node.so"
    package_library="${output_dir}/graft-sdk.linux-arm64-gnu.node"
    ;;
  x86_64-unknown-linux-gnu)
    cargo_library="${target_dir}/${rust_target}/release/libgraft_sdk_node.so"
    package_library="${output_dir}/graft-sdk.linux-x64-gnu.node"
    ;;
  x86_64-pc-windows-msvc)
    cargo_library="${target_dir}/${rust_target}/release/graft_sdk_node.dll"
    package_library="${output_dir}/graft-sdk.win32-x64-msvc.node"
    ;;
  *)
    echo "Unsupported SDK build target: ${rust_target}" >&2
    exit 1
    ;;
esac

cargo build --manifest-path "${workspace_dir}/Cargo.toml" \
  --package graft-sdk-node \
  --release \
  --target "${rust_target}" \
  --locked

mkdir -p "${output_dir}"
cp "${cargo_library}" "${package_library}"
case "${rust_target}" in
  aarch64-apple-darwin|x86_64-apple-darwin)
    # Rust's linker-generated ad-hoc signature can retain the pre-link file
    # coverage after incremental release builds. Re-sign the final Node
    # artifact so dyld validates the complete copied file.
    codesign --force --sign - "${package_library}"
    ;;
esac
echo "${package_library}"
