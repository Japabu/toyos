#!/bin/sh
# Put this checkout's build outputs directly in a content-keyed directory on
# the persistent runner. There is no restore or save transfer: Cargo updates
# the exact cache in place, and another toolchain/lockfile pair gets another
# directory. This script is reached only when /toyos-cache is mounted by a
# trusted T14 job.
set -eu

: "${TAG:?the toolchain tag is required}"
cache=/toyos-cache

case "$TAG" in
  toolchain-linux-x86_64-[0-9a-f][0-9a-f]*) ;;
  *) echo "::error::refusing malformed toolchain cache tag: $TAG"; exit 1 ;;
esac

lock_hash=$(
  git ls-files '*Cargo.lock' \
    | LC_ALL=C sort \
    | while IFS= read -r lock; do sha256sum "$lock"; done \
    | sha256sum \
    | cut -c1-16
)
key="guest-$TAG-$lock_hash"
root="$cache/build/$key"
mkdir -p "$cache/cargo" "$root"
touch "$root/.last-used"

link_target() {
  work=$1
  saved="$root/$work"
  if [ -e "$work" ] || [ -L "$work" ]; then
    echo "::error::$work exists before the local cache is linked"
    exit 1
  fi
  mkdir -p "$(dirname "$work")" "$saved"
  ln -s "$saved" "$work"
}

link_target target
link_target kernel/target
link_target bootloader/target
link_target userland/target
link_target tests/target

find tests/toyos-rust-tests -mindepth 1 -maxdepth 1 -type d -print \
  | LC_ALL=C sort \
  | while IFS= read -r crate; do link_target "$crate/target"; done

echo "local build cache: $key"

