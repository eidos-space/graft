#!/usr/bin/env bash
# Run from the repository root. Images and public packages are prepared by CI.
set -euo pipefail
: "${GRAFT_TEST_IMAGE:?Set the exact server image to verify}"
: "${GRAFT_S3_TEST_IMAGE:?Build the pinned official-source S3 fixture first}"
: "${GRAFT_PUBLIC_SDK_ROOT:?Set the installed public SDK package path}"
: "${GRAFT_PUBLIC_NATIVE_NAME:?Set the matching published native package suffix}"
export GRAFT_CANDIDATE_SDK_PATH="$GRAFT_PUBLIC_SDK_ROOT"
export GRAFT_CANDIDATE_NATIVE_PATH="$(find "$(dirname "$GRAFT_PUBLIC_SDK_ROOT")/graft-$GRAFT_PUBLIC_NATIVE_NAME" -name '*.node' -print -quit)"
test -f "$GRAFT_CANDIDATE_NATIVE_PATH"
export GRAFT_TEST_SKIP_SDK=1 GRAFT_SDK_RELEASED=1 GRAFT_LARGE_PAYLOAD_MIB=80 GRAFT_LARGE_DATABASE_MIB=80
export GRAFT_CANDIDATE_SCENARIOS=none,interrupt-upload,lost-object-response,lost-ref-response,server-crash,client-crash,interrupt-multipart-part,lost-multipart-complete
for mode in local s3; do
  if [ "$mode" = s3 ]; then export GRAFT_TEST_S3=1; else export GRAFT_TEST_S3=0; fi
  df -h .
  node services/graft-remote-docker/test/container.mjs 2>&1 | tee "public-sdk-$GRAFT_PUBLIC_NATIVE_NAME-$mode.log"
done
