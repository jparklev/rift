#!/usr/bin/env bash
set -euo pipefail

platform="${RIFT_NATIVE_PLATFORM:-}"
if [[ -z "${platform}" ]]; then
  case "$(uname -m)" in
    x86_64) platform="linux-x64" ;;
    aarch64 | arm64) platform="linux-arm64" ;;
    *)
      echo "Unsupported Linux architecture for Rift package E2E: $(uname -m)" >&2
      exit 1
      ;;
  esac
fi

case "${platform}" in
  linux-x64 | linux-arm64) ;;
  *)
    echo "linux-package-e2e.sh only supports Linux native packages, got ${platform}" >&2
    exit 2
    ;;
esac

case "$(uname -m):${platform}" in
  x86_64:linux-x64 | aarch64:linux-arm64 | arm64:linux-arm64) ;;
  *)
    echo "Native platform ${platform} does not match runner architecture $(uname -m)" >&2
    exit 1
    ;;
esac

output="$(mktemp -d "${PWD}/.rift-linux-package-e2e.XXXXXX")"
cleanup() {
  rm -rf "${output}"
}
trap cleanup EXIT

if [[ -n "${RIFT_NATIVE_TARBALL:-}" ]]; then
  native="${RIFT_NATIVE_TARBALL}"
  test -f "${native}"
else
  cargo build --package rift-cli --package rift-ffi --release --locked
  node scripts/prepare-npm-native.mjs \
    "npm/rift-${platform}" \
    target/release/rift \
    target/release/librift_ffi.so
  npm pack "./npm/rift-${platform}" --json --pack-destination "${output}" > "${output}/native-pack.json"
  native="${output}/$(node scripts/npm-pack-filename.mjs "${output}/native-pack.json")"
fi
npm pack ./npm/rift --json --pack-destination "${output}" > "${output}/root-pack.json"
root="${output}/$(node scripts/npm-pack-filename.mjs "${output}/root-pack.json")"

RIFT_PACKED_TEST_ROOT="${output}/test-root" \
  bash scripts/ci/packed-npm-e2e.sh \
    "${platform}" lifecycle "${root}" "${native}"
