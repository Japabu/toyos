#!/bin/sh
# The toolchain `toolchain.yml` published for this tree, installed and linked as
# `toyos`.
#
# One script rather than a copy in each of the four jobs that need it — the
# shards, `tcg`, gate A and `probe-cache` — because four copies of a retry loop
# are four things that can silently disagree about how long to wait and what a
# missing release means.
#
# **The asset and not the tag.** `gh release create` makes the release and then
# uploads 401 MiB to it, so a tag that answers 200 is not an installable
# toolchain; `ci.yml`'s `toolchain-ready` asks the same question before any of
# these jobs start, which is why the wait here is short — anything still missing
# by now is the API rather than a build.
#
# `$TAG` if the caller already computed it (a `cache` key wants it as a step
# output), otherwise the same `git rev-parse` `toolchain.yml` publishes under.
# `$GH_TOKEN` because a release asset download is authenticated. `gh` itself is
# not here: the `debian:sid` image has none.
set -eu

: "${GH_TOKEN:?the release asset download is authenticated}"

git config --global --add safe.directory "$PWD"
tag=${TAG:-}
if [ -z "$tag" ]; then
  tag=toolchain-linux-x86_64-$(git rev-parse HEAD:rust HEAD:toyos-abi/src \
        HEAD:toyos/src HEAD:userland/libc/src | sha256sum | cut -c1-16)
fi
echo "toolchain: $tag"

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

# **The retry belongs on the transfer, not only on the lookup above.** The loop
# that waits for the asset to exist costs one small JSON request; this is 401
# MiB over TLS, so it is the request that actually fails — run `31407229079`,
# shard 8: `curl: (35) TLS connect error: unexpected eof while reading`, one
# shard of thirteen, on a required check, with nothing to catch it.
#
# `--retry` alone would not have: curl retries transient *HTTP* status and
# connection refusals, and 35 is a handshake that got part-way. `--retry-all-errors`
# is what covers it, and the outer loop covers the case where curl exits after
# its own attempts are spent. The unpack is inside the loop because a truncated
# body is a `zstd` failure rather than a `curl` one, and retrying the download
# without it would install a corrupt toolchain and blame the compiler.
for attempt in 1 2 3; do
  if curl -sSL --retry 3 --retry-all-errors --retry-delay 5 \
       -H "Authorization: Bearer $GH_TOKEN" \
       -H "Accept: application/octet-stream" "$asset" -o /tmp/t.tar.zst \
     && mkdir -p rust/build \
     && zstd -dc /tmp/t.tar.zst | tar -C rust/build -x; then
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
stage2="$PWD/rust/build/x86_64-unknown-linux-gnu/stage2"
rustup toolchain link toyos "$stage2"
"$stage2/bin/rustc" -vV
