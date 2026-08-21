#!/bin/sh
# Cargo clean removes a target symlink when the external-dependency stamp is
# stale, then the build recreates that target as a directory in the checkout.
# Snapshot only those replaced directories back into the persistent cache.
# Targets whose links survived already wrote directly into the cache.
set -eu

cache=/toyos-cache
key=$(sh "$(dirname "$0")/cache-key.sh")
root="$cache/build/$key"

save_target() {
  work=$1
  saved="$root/$work"

  if [ -L "$work" ] || [ ! -d "$work" ]; then
    return
  fi

  incoming="$saved.incoming"
  rm -rf "$incoming"
  mkdir -p "$(dirname "$saved")" "$incoming"

  # Both mounts normally reach the same host filesystem, so hard links make
  # this snapshot effectively instant and consume no second copy. Keep the
  # ordinary copy fallback for a runner whose cache is moved to another disk.
  if ! cp -al "$work/." "$incoming/" 2>/dev/null; then
    rm -rf "$incoming"
    mkdir -p "$incoming"
    cp -a "$work/." "$incoming/"
  fi

  rm -rf "$saved"
  mv "$incoming" "$saved"
  echo "local build cache: saved $work"
}

save_target target
save_target kernel/target
save_target bootloader/target
save_target userland/target
save_target tests/target
save_target tests/toyos-rust-tests/target

find tests/toyos-rust-tests -mindepth 1 -maxdepth 1 -type d -print \
  | LC_ALL=C sort \
  | while IFS= read -r crate; do
      [ -f "$crate/Cargo.toml" ] || continue
      save_target "$crate/target"
    done

touch "$root/.last-used"
