#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
GUEST_DIR="${SCRIPT_DIR}/guest"
DIST_DIR="${SCRIPT_DIR}/dist"
TARGET="wasm32-wasip2"

cargo build --manifest-path "${GUEST_DIR}/Cargo.toml" --target "${TARGET}"

mkdir -p "${DIST_DIR}"
cp \
  "${GUEST_DIR}/target/${TARGET}/debug/echo_plugin_guest.wasm" \
  "${DIST_DIR}/plugin.wasm"

printf 'Built plugin component at %s\n' "${DIST_DIR}/plugin.wasm"
