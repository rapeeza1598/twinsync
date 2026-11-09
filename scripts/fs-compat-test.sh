#!/usr/bin/env bash
# Real-filesystem compatibility test using throwaway disk images.
#
# Creates small APFS / HFS+ / exFAT / FAT32 images in a scratch directory, mounts
# them hidden from Finder (-nobrowse), runs the ignored tests in
# tests/fs_compat_tests.rs, then detaches ONLY the devices this script attached.
# Between the write and verify phases each image is detached and re-attached, so
# the volume comes back with a new device number (the "replug" case).
#
# It never touches any real drive. Usage: scripts/fs-compat-test.sh [scratch-dir]
set -euo pipefail

export PATH="$HOME/.cargo/bin:$PATH"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRATCH="${1:-$(mktemp -d "${TMPDIR:-/tmp}/twinsync-fscompat.XXXXXX")}"
mkdir -p "$SCRATCH/mnt" "$SCRATCH/state" "$SCRATCH/src"
ATTACHED=()   # whole-disk devices attached by this script, and nothing else

attach() { # name -> prints the whole-disk device
  local name="$1" dev
  dev="$(hdiutil attach -nobrowse -mountpoint "$SCRATCH/mnt/$name" "$SCRATCH/$name.dmg" | awk 'NR==1{print $1}')"
  ATTACHED+=("$dev")
  echo "$dev"
}

detach_mount() { # name  (detach by our own mount point)
  local name="$1" i
  for i in 1 2 3 4 5; do
    hdiutil detach "$SCRATCH/mnt/$name" >/dev/null 2>&1 && return 0
    sleep 1
  done
  echo "could not detach $SCRATCH/mnt/$name" >&2
  return 1
}

cleanup() {
  for name in APFS HFS EXFAT FAT32; do
    if [ -d "$SCRATCH/mnt/$name" ] && mount | grep -q " on $SCRATCH/mnt/$name "; then
      detach_mount "$name" || true
    fi
  done
}
trap cleanup EXIT

FAIL=0
for spec in "APFS|APFS|TSAPFS" "HFS|Journaled HFS+|TSHFS" "EXFAT|ExFAT|TSEXFAT" "FAT32|MS-DOS FAT32|TSFAT32"; do
  IFS='|' read -r name fs volname <<<"$spec"
  echo "=== $name ($fs) ==="
  hdiutil create -quiet -size 96m -fs "$fs" -volname "$volname" "$SCRATCH/$name.dmg"
  mkdir -p "$SCRATCH/mnt/$name"
  attach "$name" >/dev/null
  export TWINSYNC_COMPAT_VOLUME="$SCRATCH/mnt/$name"
  export TWINSYNC_COMPAT_STATE="$SCRATCH/state/$name"
  export TWINSYNC_COMPAT_SRC="$SCRATCH/src/$name"
  ( cd "$ROOT" && cargo test --test fs_compat_tests -- --ignored --exact phase_write --nocapture 2>&1 | grep -E "COMPAT|test result|panicked|assert" ) || FAIL=1
  detach_mount "$name"
  attach "$name" >/dev/null          # same drive, new device number
  ( cd "$ROOT" && cargo test --test fs_compat_tests -- --ignored --exact phase_verify --nocapture 2>&1 | grep -E "COMPAT|test result|panicked|assert" ) || FAIL=1
  detach_mount "$name"
done

echo "scratch directory (safe to delete): $SCRATCH"
exit $FAIL
