#!/usr/bin/env bash
set -eu -o pipefail

function tick() {
  if test -z "${tick+set}"
  then
    tick=1112911993
  else
    tick=$(($tick + 60))
  fi
  GIT_COMMITTER_DATE="$tick -0700"
  GIT_AUTHOR_DATE="$tick -0700"
  export GIT_COMMITTER_DATE GIT_AUTHOR_DATE
}

function force_tag() {
  local name head_oid common_dir
  name=${1:?argument the tag name}

  # This should only be needed with 32-bit `git`, so fail otherwise.
  word_size="$(
    git --version --build-options |
      awk '$1 == "sizeof-size_t:" { print $2 }'
  )"
  ((word_size == 4))

  # Manually create the tag.
  head_oid="$(git rev-parse HEAD)"
  common_dir="$(git rev-parse --git-common-dir)"
  (set -o noclobber; echo "$head_oid" > "$common_dir/refs/tags/$name")
}

function tagged_commit() {
  local message=${1:?first argument is the commit message and tag name}
  local date=${2:-}
  local file="$message.t"
  echo "$1" > "$file"
  git add -- "$file"
  if [ -n "$date" ]; then
    export GIT_COMMITTER_DATE="$date"
  else
    tick
  fi
  git commit -m "$message"
  git tag -- "$message" || force_tag "$message"
}

tick

# adapted from git/t/t5318 'lower layers have overflow chunk'
UNIX_EPOCH_ZERO="@0 +0000"
FUTURE_DATE="@4147483646 +0000"

git init
git config commitGraph.generationVersion 2

tagged_commit future-1 "$FUTURE_DATE"
tagged_commit old-1 "$UNIX_EPOCH_ZERO"
git commit-graph write --reachable
tagged_commit future-2 "$FUTURE_DATE"
tagged_commit old-2 "$UNIX_EPOCH_ZERO"
git commit-graph write --reachable --split=no-merge
tagged_commit extra
# this makes sure it's actually in chain format.
git commit-graph write --reachable --split=no-merge
