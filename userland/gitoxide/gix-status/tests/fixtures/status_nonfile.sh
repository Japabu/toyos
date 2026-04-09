#!/usr/bin/env bash
set -eu -o pipefail

git init -q untracked
(cd untracked
  touch file && git add file && git commit -m "just to get an index for the test-suite"

  mkfifo pipe
  git status
)

git init -q tracked-swapped
(cd tracked-swapped
  touch file && git add file && git commit -m "it starts out as trackable file"

  rm file && mkfifo file
  git status
)

