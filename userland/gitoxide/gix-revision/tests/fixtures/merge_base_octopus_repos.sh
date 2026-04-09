#!/usr/bin/env bash
set -eu -o pipefail

git init three-sequential-commits
(cd three-sequential-commits
  git commit -m "A" --allow-empty
  git commit -m "B" --allow-empty
  git commit -m "C" --allow-empty
)

git init three-parallel-commits
(cd three-parallel-commits
  git commit -m "BASE" --allow-empty
  git branch A
  git branch B
  git branch C

  git checkout A
  git commit -m "A" --allow-empty

  git checkout B
  git commit -m "B" --allow-empty

  git checkout C
  git commit -m "C" --allow-empty
)

git init three-forked-commits
(cd three-forked-commits
  git commit -m "BASE" --allow-empty
  git branch A

  git checkout -b C
  git commit -m "C" --allow-empty

  git checkout A
  git commit -m "A-1" --allow-empty
  git branch B
  git commit -m "A-2" --allow-empty

  git checkout B
  git commit -m "B" --allow-empty
)
