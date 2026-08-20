#!/bin/sh
# The toolchain `toolchain.yml` published for this tree, installed and linked
# as `toyos`. One script rather than a copy in each caller (the shards, `tcg`,
# `cache-writer`, gate A), so a retry-loop disagreement is not possible.
#
# The asset, not the tag: `gh release create` makes the release and then
# uploads to it, so a tag that answers 200 is not an installable toolchain.
# `ci.yml`'s `toolchain-ready` asks the same question before any of these jobs
# start, so the wait here is short — anything still missing is the API rather
# than a build.
#
# `$TAG` if the caller already computed it (a `cache` key wants it as a step
# output), otherwise the same `git rev-parse` `toolchain.yml` publishes under.
# `$GH_TOKEN` authenticates the release download; `gh` itself is not here —
# the `debian:sid` image has none.
set -eu

: "${GH_TOKEN:?the release asset download is authenticated}"

git config --global --add safe.directory "$PWD"
tag=${TAG:-}
if [ -z "$tag" ]; then
  tag=toolchain-linux-x86_64-$(git rev-parse HEAD:rust HEAD:toyos-abi/src \
        HEAD:toyos/src HEAD:userland/libc/src | sha256sum | cut -c1-16)
fi
echo "toolchain: $tag"

# The T14 image mounts a runner-local cache here. A complete content-keyed
# entry is linked in place without an API call, a 401 MiB download or another
# extraction. GitHub-hosted containers do not set TOYOS_LOCAL_CACHE and retain
# the established release-download path below.
cache=${TOYOS_LOCAL_CACHE:-}
cache_entry=""

# The build system intentionally verifies the literal rustup link target and
# uses both of the artifact's build triples. Keep the complete rust/build tree
# at its checkout path through one symlink while storing its bytes once in the
# local cache.
link_toolchain() {
  source_stage2=$1
  linked_stage2=$source_stage2
  if [ -n "$cache" ]; then
    linked_build="$PWD/rust/build"
    linked_stage2="$linked_build/x86_64-unknown-linux-gnu/stage2"
    mkdir -p "$PWD/rust"
    if [ -L "$linked_build" ]; then
      rm -f "$linked_build"
    elif [ -d "$linked_build" ]; then
      # A pre-cache job can leave the extracted artifact here. The complete
      # cached source above is the same content-addressed toolchain, so discard
      # only this duplicate before replacing it with the stable link.
      echo "replacing duplicate workspace toolchain with the local-cache link"
      find "$linked_build" -mindepth 1 -delete
      rmdir "$linked_build"
    elif [ -e "$linked_build" ]; then
      echo "::error::$linked_build exists and is not the local-cache link"
      exit 1
    fi
    ln -s "$cache_entry" "$linked_build"
  fi
  rustup toolchain link toyos "$linked_stage2"
  "$source_stage2/bin/rustc" -vV
}

if [ -n "$cache" ]; then
  case "$tag" in
    toolchain-linux-x86_64-[0-9a-f][0-9a-f]*) ;;
    *) echo "::error::refusing malformed toolchain cache tag: $tag"; exit 1 ;;
  esac
  cache_entry="$cache/toolchains/$tag"
  stage2="$cache_entry/x86_64-unknown-linux-gnu/stage2"
  if [ -f "$cache_entry/.complete" ] && [ -x "$stage2/bin/rustc" ]; then
    echo "local toolchain cache hit: $tag"
    link_toolchain "$stage2"
    exit 0
  fi
  mkdir -p "$cache_entry"
  find "$cache_entry" -mindepth 1 -delete
fi

api="https://api.github.com/repos/${GITHUB_REPOSITORY}/releases/tags/$tag"
asset=""
for _ in $(seq 10); do
  asset=$(curl -sSL -H "Authorization: Bearer $GH_TOKEN" "$api" \
    | jq -r '.assets[]? | select(.name=="toyos-toolchain.tar.zst") | .url')
  if [ -n "$asset" ]; then
    break
  fi
  echo "$tag does not carry toyos-toolchain.tar.zst yet; retrying"
  sleep 15
done

if [ -z "$asset" ]; then
  echo "::error::$tag carries no toyos-toolchain.tar.zst, so there is nothing to install."
  echo "::error::toolchain.yml publishes it on a pull request and on a push to main, and"
  echo "::error::nothing else builds one. A dispatch against a ref that has never been"
  echo "::error::through it lands here."
  exit 1
fi

# The retry belongs on the transfer, not only on the lookup above: this is
# 401 MiB over TLS, so a handshake that gets part-way is the failure mode.
# `--retry-all-errors` covers it, where plain `--retry` only covers transient
# HTTP status and connection refusals; the outer loop covers curl exiting
# after its own attempts are spent. The unpack is inside the loop because a
# truncated body is a `zstd` failure rather than a `curl` one, and retrying
# without it would install a corrupt toolchain and blame the compiler.
extract_root=rust/build
[ -z "$cache_entry" ] || extract_root=$cache_entry
for attempt in 1 2 3; do
  if curl -sSL --retry 3 --retry-all-errors --retry-delay 5 \
       -H "Authorization: Bearer $GH_TOKEN" \
       -H "Accept: application/octet-stream" "$asset" -o /tmp/t.tar.zst \
     && mkdir -p "$extract_root" \
     && zstd -dc /tmp/t.tar.zst | tar -C "$extract_root" -x; then
    break
  fi
  if [ "$attempt" = 3 ]; then
    echo "::error::the toolchain asset did not download and unpack in three attempts."
    echo "::error::The last failure is above; this is the transfer, not the build."
    exit 1
  fi
  echo "toolchain download/unpack attempt $attempt failed; retrying"
  rm -f /tmp/t.tar.zst
  sleep 10
done
stage2="$extract_root/x86_64-unknown-linux-gnu/stage2"
if [ -n "$cache_entry" ]; then
  "$stage2/bin/rustc" -vV
  touch "$cache_entry/.complete"
  echo "local toolchain cache filled: $tag"
fi
link_toolchain "$stage2"
