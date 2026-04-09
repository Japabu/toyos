#!/usr/bin/env bash

set -eux -o pipefail

root="$1"
output_corpus="$2"
fixtures_dir="$(readlink -f -- "$root/gix-object/tests/fixtures/tree")"

echo "$root"
echo "$fixtures_dir"

zip -j "$output_corpus" "$fixtures_dir"/*
