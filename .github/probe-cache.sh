#!/bin/sh
# One arm of `probe-cache.yml`: build the tree the way a `ci.yml` shard does,
# say how long it took, and say whether a restored `target/` survived contact
# with the build system's own invalidation.
#
# `$1` is the arm's name and appears in the job summary; nothing else differs
# between the two.
set -eu

arm=$1
log=/tmp/$arm.log

t0=$(date +%s)
set +e
cargo test --test toyos-build -- --jobs 1 --host-slots 0 process_stats > "$log" 2>&1
status=$?
set -e
elapsed=$(( $(date +%s) - t0 ))

# Where cargo stopped compiling and the harness started, off the log's own
# markers: `Finished` is the last thing cargo prints before the build system
# takes over, and `running N tests` is libtest.
compiles=$(grep -c '^ *Compiling ' "$log" || true)
cleans=$(grep -c 'external deps changed: cleaning' "$log" || true)

# The target directories this tree actually has, for the arm that prices the
# cache entry. Written rather than guessed, because a path the cache does not
# name is a path the warm arm rebuilds.
#
# To stdout as well as to the summary: a job summary is not readable from the
# REST API, so a number that lives only there cannot be quoted afterwards.
find . -maxdepth 3 -name target -type d -not -path './rust/*' > /tmp/target-dirs
echo "PROBE-CACHE $arm TARGET-DIRS:"
du -sh $(cat /tmp/target-dirs) 2>/dev/null | sort -h

{
  echo "- **$arm**: ${elapsed}s, exit $status, $compiles crate compiles, $cleans external-dep cleans"
} >> "${GITHUB_STEP_SUMMARY:-/dev/null}"

echo "PROBE-CACHE $arm SECONDS: $elapsed"
echo "PROBE-CACHE $arm COMPILES: $compiles"
echo "PROBE-CACHE $arm CLEANS: $cleans"
tail -60 "$log"
exit "$status"
