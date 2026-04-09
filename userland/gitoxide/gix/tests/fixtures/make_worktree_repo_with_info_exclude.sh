#!/usr/bin/env bash
set -eu -o pipefail

git init -q repo
(cd repo
  git checkout -b main
  touch a b
  git add .
  git commit -m c1
  git worktree add ../worktree
  echo ignored-file > .git/info/exclude
)
