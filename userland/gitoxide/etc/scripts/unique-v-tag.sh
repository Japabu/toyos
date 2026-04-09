#!/usr/bin/env bash

set -efu
IFS=$'\n'

# shellcheck disable=SC2207  # Intentionally splitting. No globbing due to set -f.
tags=(
    $(git tag --points-at HEAD -- 'v*')
)

count="${#tags[@]}"
if ((count != 1)); then
    printf '%s: error: Found %d matching v* tags, need exactly 1.\n' "$0" "$count" >&2
    exit 1
fi

printf '%s\n' "${tags[0]}"
