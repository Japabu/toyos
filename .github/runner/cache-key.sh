#!/bin/sh
# The one derivation of a persistent-cache key, so `link-build-cache.sh` and
# `save-build-cache.sh` cannot disagree about where a target directory went —
# a disagreement would silently lose a build rather than fail.
#
#   sh cache-key.sh          # validate $TAG and print the key
#   sh cache-key.sh check    # validate $TAG and print nothing
#
# The tag is a path component. `$cache/toolchains/$TAG` and
# `$cache/build/guest-$TAG-<lock>` are directories these scripts create,
# delete into and `mv` over, so anything the tag can hold, a path can hold.
# It arrives as a workflow step output computed with `sha256sum | cut -c1-16`,
# which is sixteen hexadecimal characters — and that is what is checked, all of
# it. The pattern this replaces, `toolchain-linux-x86_64-[0-9a-f][0-9a-f]*`,
# constrained the first two characters and nothing after them, so
# `toolchain-linux-x86_64-ab/../../../../home/t14/.ssh` passed it and named a
# directory outside the cache. Nothing untrusted reaches this today; a check
# that reads like a boundary and is not one is worse than no check.
set -eu

mode=${1:-key}

: "${TAG:?the toolchain tag is required}"

refuse() {
  echo "::error::refusing malformed toolchain cache tag: $TAG"
  exit 1
}

hash=${TAG#toolchain-linux-x86_64-}
[ "$hash" != "$TAG" ] || refuse
case "$hash" in
  ????????????????) ;;
  *) refuse ;;
esac
case "$hash" in
  *[!0-9a-f]*) refuse ;;
esac

[ "$mode" != check ] || exit 0
[ "$mode" = key ] || { echo "usage: $0 [key|check]" >&2; exit 2; }

# Every lockfile in the tree, in a fixed order: the toolchain tag decides the
# sysroot and the lockfiles decide the dependency graph, and a target directory
# is reusable only when both agree.
lock_hash=$(
  git ls-files '*Cargo.lock' \
    | LC_ALL=C sort \
    | while IFS= read -r lock; do sha256sum "$lock"; done \
    | sha256sum \
    | cut -c1-16
)

echo "guest-$TAG-$lock_hash"
