#!/bin/sh
# One standing issue for a nightly red, found by title and updated rather than
# recreated. Shared by ci.yml's and host-tests.yml's own `nightly-red` jobs so
# the two do not carry two slightly different copies of the same "find or
# file" logic.
#
# A green scheduled run never calls this: both callers gate on their own run's
# result before they do, so this script has no green path to be silent on.
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
