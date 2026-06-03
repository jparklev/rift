#!/usr/bin/env bash
set -euo pipefail

filesystem="${1:?usage: linux-fs.sh <btrfs|xfs-reflink|xfs-no-reflink|ext4|tmpfs|zfs|bcachefs>}"
workspace="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
runner_temp="${RUNNER_TEMP:-$(mktemp -d)}"
mountpoint="/mnt/rift-${filesystem}"
checkout="${mountpoint}/rift"
image="${runner_temp}/rift-${filesystem}.img"

sudo mkdir -p "${mountpoint}"

copy_checkout() {
  sudo chown "${USER}:${USER}" "${mountpoint}"
  mkdir -p "${checkout}"
  cp -a "${workspace}/." "${checkout}"
}

format_loopback() {
  local size="$1"
  shift

  truncate -s "${size}" "${image}"
  "$@" "${image}"
}

mount_loopback() {
  sudo mount -o loop "${image}" "${mountpoint}"
}

case "${filesystem}" in
  btrfs)
    format_loopback 1G mkfs.btrfs -f
    mount_loopback
    ;;
  xfs-reflink)
    format_loopback 1G mkfs.xfs -f -m reflink=1
    mount_loopback
    ;;
  xfs-no-reflink)
    format_loopback 1G mkfs.xfs -f -m reflink=0
    mount_loopback
    ;;
  ext4)
    format_loopback 1G mkfs.ext4 -F
    mount_loopback
    ;;
  tmpfs)
    sudo mount -t tmpfs -o size=1G tmpfs "${mountpoint}"
    ;;
  zfs)
    pool="rift-ci-${GITHUB_RUN_ID:-$$}-${RANDOM}"
    truncate -s 1G "${image}"
    sudo modprobe zfs || true
    sudo zpool create -f -m "${mountpoint}" "${pool}" "${image}"
    ;;
  bcachefs)
    truncate -s 1G "${image}"
    sudo modprobe bcachefs || true
    loopdev="$(sudo losetup --find --show "${image}")"
    if command -v bcachefs >/dev/null 2>&1; then
      sudo bcachefs format "${loopdev}"
    elif command -v mkfs.bcachefs >/dev/null 2>&1; then
      sudo mkfs.bcachefs "${loopdev}"
    else
      echo "bcachefs formatting tool is unavailable" >&2
      exit 1
    fi
    sudo mount -t bcachefs "${loopdev}" "${mountpoint}" \
      || sudo bcachefs mount "${loopdev}" "${mountpoint}"
    ;;
  *)
    echo "unsupported filesystem fixture: ${filesystem}" >&2
    exit 2
    ;;
esac

copy_checkout
