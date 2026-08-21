#!/usr/bin/env bash
# The T14 runner's job-start hook: the whole trust boundary of this appliance.
# Actions runs it, as the runner account, before any of a job's own steps, and
# a non-zero exit refuses the job. Everything downstream — the container, the
# persistent build cache, the accelerator — assumes what this file admitted.
#
# It cannot be narrower than it is. The runner mounts /var/run/docker.sock into
# every container job, so an admitted job is root-capable on this host no
# matter what user its container runs as; the container user decides who owns
# the files, not who could take the machine. `.github/runner/README.md` states
# that plainly, and `install-hook.sh` beside this file is how an edit here
# reaches /usr/local/libexec/toyos-runner.
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

# Nothing is repaired here, and that is the change worth naming. Container jobs
# used to run as root and leave root-owned trees in the work area that the
# runner account could neither clean nor overwrite, so this hook ran a second,
# root, `ubuntu:24.04` container on every admission to chown them back. The
# trusted image now carries `USER ci` at this uid
# (`.github/runner/Dockerfile`), so a job owns what it writes and there is
# nothing left to repair.
#
# The uid stays asserted: the image's user is fixed at 1000, and a runner
# account that is not that uid would write a work area its own jobs cannot
# touch — one line here instead of that failure spread across a suite.
[[ "$(id -u)" == 1000 ]] || refuse "runner uid is no longer the configured 1000"
