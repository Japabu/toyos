#!/usr/bin/env bash
set -eu -o pipefail

git init -q

mkdir tracked target
echo "content" > tracked/file
echo "other" > target/file

git add tracked/file
git commit -q -m init

rm -rf tracked
ln -s target tracked
