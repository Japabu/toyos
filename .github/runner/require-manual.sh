#!/usr/bin/env bash
set -euo pipefail

if [[ "${GITHUB_REPOSITORY:-}" != "ToyOSOrg/ToyOS" ]]; then
  echo "::error::t14 accepts jobs only from ToyOSOrg/ToyOS"
  exit 1
fi

if [[ "${GITHUB_EVENT_NAME:-}" != "workflow_dispatch" ]]; then
  echo "::error::t14 accepts workflow_dispatch only; ${GITHUB_EVENT_NAME:-unknown} is refused"
  exit 1
fi

case "${GITHUB_WORKFLOW_REF:-}" in
  ToyOSOrg/ToyOS/.github/workflows/*@refs/heads/main)
    ;;
  *)
    echo "::error::t14 accepts only workflows dispatched from refs/heads/main"
    exit 1
    ;;
esac

