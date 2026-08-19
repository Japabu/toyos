#!/bin/sh
# One standing issue for a nightly red, found by title and updated rather than
# recreated. Shared by every nightly-red job in ci.yml, gate-a.yml and
# host-tests.yml so they do not carry separate copies of the same find-or-file
# logic. Every caller gates on its own run's result before calling this, so a
# green scheduled run never reaches it.
#
# The issue is the alarm and not the record: every nightly red is adjudicated
# the same day into a fix, a `src/redlist.rs` row, or a tier correction.
#
# $TITLE names the issue; $BODY becomes a comment on it, or the body of a new
# one if none is open yet. $GH_TOKEN is what `gh issue` authenticates with.
set -eu

: "${GH_TOKEN:?filing or updating the issue is authenticated}"
: "${TITLE:?}"
: "${BODY:?}"

# /bin/sh has no `pipefail`, so a `gh` piped straight into `head` would let a
# `gh` failure fall through as an empty `number` — read as "no open issue" and
# answered by filing a duplicate. `gh issue list` runs alone below so its own
# exit status is what `set -e` sees; only the already-captured text is piped.
found=$(gh issue list --state open --search "in:title \"$TITLE\"" --limit 30 \
          --json number,title --jq ".[] | select(.title == \"$TITLE\") | .number")
number=$(printf '%s\n' "$found" | head -1)

if [ -n "$number" ]; then
  echo "updating #$number"
  gh issue comment "$number" --body "$BODY"
else
  echo "filing a new issue"
  gh issue create --title "$TITLE" --body "$BODY"
fi
