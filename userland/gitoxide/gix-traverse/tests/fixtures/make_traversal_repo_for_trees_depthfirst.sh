#!/usr/bin/env bash
set -eu -o pipefail

git init -q

git checkout -q -b main
touch a b c
mkdir d e f
touch d/a e/b f/c f/z
mkdir f/ISSUE_TEMPLATE
touch f/ISSUE_TEMPLATE/x f/FUNDING.yml f/dependabot.yml

git add .
git commit -q -m c1
