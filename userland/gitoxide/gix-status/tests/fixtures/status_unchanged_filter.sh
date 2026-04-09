#!/usr/bin/env bash
set -eu -o pipefail

git init -q

touch empty
echo -n "content" >executable
chmod +x executable

mkdir dir
echo "other content" >dir/content
seq 5 >dir/content2
mkdir dir/sub-dir
(cd dir/sub-dir && ln -sf ../content symlink)

git add -A
git update-index --chmod=+x executable  # For Windows.
git commit -m "Commit"

git ls-files | xargs rm

git config core.autocrlf true
git checkout -f HEAD
