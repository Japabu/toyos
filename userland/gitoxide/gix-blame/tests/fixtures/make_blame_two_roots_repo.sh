#!/usr/bin/env bash
set -eu -o pipefail

git init -q
git config --local diff.algorithm histogram

git config merge.ff false

git checkout -q -b main

seq 1 4 > unrelated-file.txt
git add unrelated-file.txt
git commit -q -m c1

seq 1 4 > file-with-two-roots.txt
git add file-with-two-roots.txt
git commit -q -m c2

seq 1 5 > file-with-two-roots.txt
git add file-with-two-roots.txt
git commit -q -m c3

git checkout -b different-branch
git reset --hard HEAD~2

seq 4 6  > file-with-two-roots.txt
git add file-with-two-roots.txt
git commit -q -m c10

seq 4 8 > file-with-two-roots.txt
git add file-with-two-roots.txt
git commit -q -m c11

git checkout main
git merge different-branch || true
seq 1 8 > file-with-two-roots.txt
git add file-with-two-roots.txt
git commit -q -m c20

git blame --porcelain file-with-two-roots.txt > .git/file-with-two-roots.baseline
