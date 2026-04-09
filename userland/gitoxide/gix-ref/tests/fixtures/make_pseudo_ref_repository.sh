#!/usr/bin/env bash
set -eu -o pipefail

git init -q
git commit -m "init" --allow-empty

git rev-parse HEAD > .git/JIRI_HEAD
touch .git/SOME_ALL_CAPS_FILE
touch .git/refs/SHOULD_BE_EXCLUDED_HEAD

cat <<EOF >> .git/FETCH_HEAD
9064ea31fae4dc59a56bdd3a06c0ddc990ee689e		branch 'main' of https://github.com/Byron/gitoxide
1b8d9e6a408e480ae1912e919c37a26e5c46639d	not-for-merge	branch 'faster-discovery' of https://github.com/Byron/gitoxide
EOF