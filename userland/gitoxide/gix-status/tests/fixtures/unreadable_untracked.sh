#!/usr/bin/env bash
set -eu -o pipefail

git init
>tracked
git add tracked && git commit -m "init"

>unreadable
chmod 000 unreadable

git status