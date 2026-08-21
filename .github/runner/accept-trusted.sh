#!/usr/bin/env bash
set -euo pipefail

refuse() {
  echo "::error::t14 refused ${GITHUB_EVENT_NAME:-unknown}: $1"
  exit 1
}

[[ "${GITHUB_REPOSITORY:-}" == "ToyOSOrg/ToyOS" ]] \
  || refuse "repository is not ToyOSOrg/ToyOS"

main_workflow_ref() {
  case "${GITHUB_WORKFLOW_REF:-}" in
    ToyOSOrg/ToyOS/.github/workflows/*@refs/heads/main) return 0 ;;
    *) return 1 ;;
  esac
}

case "${GITHUB_EVENT_NAME:-}" in
  pull_request)
    [[ -r "${GITHUB_EVENT_PATH:-}" ]] \
      || refuse "pull-request event payload is unavailable"
    jq -e --arg repository "$GITHUB_REPOSITORY" \
      '.pull_request.head.repo.full_name == $repository' \
      "$GITHUB_EVENT_PATH" >/dev/null \
      || refuse "pull-request head is outside ToyOSOrg/ToyOS"
    ;;
  push)
    ;;
  schedule)
    main_workflow_ref || refuse "scheduled workflow is not from main"
    ;;
  workflow_dispatch)
    main_workflow_ref || refuse "dispatched workflow is not from main"
    ;;
  *)
    refuse "event type is not trusted on this persistent runner"
    ;;
esac

# Container jobs create build directories as root, and cache extraction can
# narrow inherited ACL masks back to read-only. Normalize directories after
# admission but before checkout. Deleting a tree needs write access to every
# parent directory; individual files need not be rewritten or traversed by
# chown. The fixed, root-owned hook supplies the command and mounts only this
# repository runner's work area, with networking disabled.
[[ "$(id -u)" == 1000 ]] || refuse "runner uid is no longer the configured 1000"
runner_work=/home/t14/actions-runner/_work
if [[ -d "$runner_work" ]]; then
  /usr/bin/docker run --rm --network none \
    --volume "$runner_work:/work" \
    ubuntu:24.04 \
    find /work/ToyOS /work/_temp -type d \
      -exec chown 1000:1000 '{}' + \
      -exec chmod u+rwx '{}' +
fi
