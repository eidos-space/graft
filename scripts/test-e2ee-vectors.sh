#!/usr/bin/env bash
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1

workspace_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
task_directory="$(mktemp -d "${TMPDIR:-/tmp}/graft-e2ee-vectors.XXXXXX")"
trap 'rm -rf -- "$task_directory"' EXIT
verifier_python="${GRAFT_E2EE_VERIFY_PYTHON:-python3}"
cargo_features="${GRAFT_E2EE_CARGO_FEATURES:-e2ee}"
cd "$workspace_dir"

"$verifier_python" tests/e2ee/generate_vectors.py --check
cargo test -p graft --features "$cargo_features" --test e2ee_vectors --locked
GRAFT_E2EE_VECTOR_OUTPUT="$task_directory/native.json" \
  cargo test -p graft --features "$cargo_features" --test e2ee_vectors --locked \
  export_native_envelopes_for_independent_python_verification -- --ignored --exact
"$verifier_python" tests/e2ee/test_verifier.py --native "$task_directory/native.json"
