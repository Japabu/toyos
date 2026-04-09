#!/usr/bin/env bash
set -eu -o pipefail

git init untouched

git init changed-headref
(cd changed-headref
  echo "ref: refs/heads/other" >.git/HEAD
)

git init detached
(cd detached
  echo "abcdefabcdefabcdefabcdefabcdefabcdefabcd" >.git/HEAD
)

git init invalid-loose-ref
(cd invalid-loose-ref
  touch .git/refs/heads/empty
)
