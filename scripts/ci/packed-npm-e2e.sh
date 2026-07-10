#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: packed-npm-e2e.sh <platform> <lifecycle|unavailable> <root-tarball> [native-tarball]" >&2
}

if [[ $# -lt 3 || $# -gt 4 ]]; then
  usage
  exit 2
fi

platform="$1"
mode="$2"
root_tarball="$3"
native_tarball="${4:-}"

case "${mode}" in
  lifecycle | unavailable) ;;
  *)
    usage
    exit 2
    ;;
esac

[[ -f "${root_tarball}" ]] || {
  echo "root tarball does not exist: ${root_tarball}" >&2
  exit 1
}
if [[ -n "${native_tarball}" && ! -f "${native_tarball}" ]]; then
  echo "native tarball does not exist: ${native_tarball}" >&2
  exit 1
fi

base="${RIFT_PACKED_TEST_ROOT:-$(mktemp -d)}"
mkdir -p "${base}"
output="$(mktemp -d "${base%/}/rift-packed-npm-e2e.XXXXXX")"
consumer="${output}/consumer"
test_root="${output}/test-root"

cleanup() {
  rm -rf "${output}"
}
trap cleanup EXIT

mkdir -p "${consumer}"
node scripts/verify-npm-package.mjs "${root_tarball}"

if [[ -n "${native_tarball}" ]]; then
  node scripts/verify-npm-native-package.mjs "${native_tarball}" "${platform}"
  npm install --prefix "${consumer}" --ignore-scripts --omit=optional "${root_tarball}"
else
  # This branch intentionally allows npm to resolve the platform's optional
  # dependency from the registry, as a consumer would after publication.
  npm install --prefix "${consumer}" --ignore-scripts "${root_tarball}"
  test -d "${consumer}/node_modules/@jparklev/rift-${platform}"
  for other_platform in \
    darwin-arm64 \
    darwin-x64 \
    linux-arm64 \
    linux-x64 \
    windows-arm64 \
    windows-x64; do
    if [[ "${other_platform}" != "${platform}" ]]; then
      test ! -e "${consumer}/node_modules/@jparklev/rift-${other_platform}"
    fi
  done
fi

cp \
  scripts/test-missing-native.mjs \
  scripts/test-packed-api.mjs \
  scripts/test-packed-cli.mjs \
  scripts/test-packed-unavailable-api.mjs \
  scripts/test-packed-unavailable-cli.mjs \
  "${consumer}/"

if [[ -n "${native_tarball}" ]]; then
  (
    cd "${consumer}"
    node --experimental-ffi ./test-missing-native.mjs
  )
  npm install --prefix "${consumer}" --ignore-scripts --omit=optional "${native_tarball}"
fi

(
  cd "${consumer}"
  if [[ "${mode}" == "lifecycle" ]]; then
    RIFT_PACKED_TEST_ROOT="${test_root}" node --experimental-ffi ./test-packed-api.mjs
    RIFT_PACKED_TEST_ROOT="${test_root}" bun ./test-packed-api.mjs
    RIFT_PACKED_TEST_ROOT="${test_root}" node ./test-packed-cli.mjs
  else
    RIFT_PACKED_TEST_ROOT="${test_root}" node --experimental-ffi ./test-packed-unavailable-api.mjs
    RIFT_PACKED_TEST_ROOT="${test_root}" bun ./test-packed-unavailable-api.mjs
    RIFT_PACKED_TEST_ROOT="${test_root}" node ./test-packed-unavailable-cli.mjs
  fi
)
