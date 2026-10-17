#!/usr/bin/env bash
set -euo pipefail

workspace_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
task_directory="$(mktemp -d "${TMPDIR:-/tmp}/graft-e2ee-node.XXXXXX")"
trap 'rm -rf "$task_directory"' EXIT
target_dir="${CARGO_TARGET_DIR:-${workspace_dir}/target}"
case "$(uname -s)" in
  Darwin) features="macos-keychain"; library="libgraft_sdk_node.dylib" ;;
  Linux) features="e2ee"; library="libgraft_sdk_node.so" ;;
  *) echo "Approved native checkpoint provider unavailable on this host" >&2; exit 1 ;;
esac
cd "$workspace_dir"
pnpm --filter @eidos.space/graft-remote build
cargo build -p graft-sdk-node --features "$features" --locked
cp "$target_dir/debug/$library" "$task_directory/graft-sdk.node"
if [[ "$(uname -s)" == "Darwin" ]]; then
  codesign --force --sign - "$task_directory/graft-sdk.node"
fi
GRAFT_E2EE_NODE_TEST_BINDING="$task_directory/graft-sdk.node" \
  cargo test -p graft-sdk --features e2ee --test e2ee_node --locked -- --ignored
GRAFT_SDK_NATIVE_PATH="$task_directory/graft-sdk.node" pnpm --dir packages/graft-sdk test
cargo build -p graft-sdk-node --locked
cp "$target_dir/debug/$library" "$task_directory/graft-sdk-plain.node"
if [[ "$(uname -s)" == "Darwin" ]]; then
  codesign --force --sign - "$task_directory/graft-sdk-plain.node"
fi
GRAFT_SDK_NATIVE_PATH="$task_directory/graft-sdk-plain.node" node tests/e2ee/node-feature-off.cjs
