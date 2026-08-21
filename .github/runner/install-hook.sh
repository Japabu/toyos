#!/bin/sh
# Place the runner's job-start hook and register it. Run as root on the runner
# host:
#
#   sudo sh .github/runner/install-hook.sh [accept-trusted|require-manual]
#
# The hook is the whole trust boundary of this appliance — it is what decides
# which GitHub event may run a job on a machine that keeps state between jobs —
# and it lived only on the laptop until this directory existed. The runner user
# has no sudo by design, so this is the one step of the appliance an operator
# performs by hand; everything else is a workflow.
#
# `accept-trusted` (the default) admits the event set every routed workflow
# depends on. `require-manual` is the narrower predecessor: dispatches from
# `main` only, and nothing else. Re-running this script is how an edit to
# either file reaches the machine, and `README.md` beside it has the recipe
# that reads back what is actually installed.
set -eu

hook=${1:-accept-trusted}
case "$hook" in
  accept-trusted | require-manual) ;;
  *)
    echo "usage: $0 [accept-trusted|require-manual]" >&2
    exit 1
    ;;
esac

[ "$(id -u)" = 0 ] || {
  echo "$0 writes /usr/local/libexec and must run as root" >&2
  exit 1
}

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
dest=/usr/local/libexec/toyos-runner

# The account the runner service runs as. Its home holds the runner, its work
# area and the persistent cache, and its uid is what the job containers run as
# — `accept-trusted.sh` asserts the same number, so the two cannot drift.
runner_user=${RUNNER_USER:-t14}
runner_uid=$(id -u "$runner_user")
runner_gid=$(id -g "$runner_user")
runner_home=$(getent passwd "$runner_user" | cut -d: -f6)
[ -n "$runner_home" ] || { echo "$runner_user has no home directory" >&2; exit 1; }
runner_dir=$runner_home/actions-runner
cache_dir=$runner_home/actions-runner-cache

[ -d "$runner_dir" ] || { echo "$runner_dir is not there" >&2; exit 1; }

install -d -o root -g root -m 0755 "$dest"
for script in accept-trusted.sh require-manual.sh; do
  install -o root -g root -m 0755 "$here/$script" "$dest/$script"
done

# The registration, rewritten rather than appended to: a second
# ACTIONS_RUNNER_HOOK_JOB_STARTED line in this file is a hook nobody can see.
env_file=$runner_dir/.env
touch "$env_file"
tmp=$env_file.installing
grep -v '^ACTIONS_RUNNER_HOOK_JOB_STARTED=' "$env_file" > "$tmp" || true
echo "ACTIONS_RUNNER_HOOK_JOB_STARTED=$dest/$hook.sh" >> "$tmp"
install -o "$runner_uid" -g "$runner_gid" -m 0644 "$tmp" "$env_file"
rm -f "$tmp"

# The job containers run unprivileged as this uid (`.github/runner/Dockerfile`
# builds the image that way), so everything they write must already belong to
# it. Earlier revisions ran them as root, which is why this is a repair and not
# only an initialisation — a root-owned directory under the cache refuses every
# unprivileged write that follows.
for owned in "$cache_dir" "$runner_dir/_work"; do
  [ -d "$owned" ] || continue
  chown -R "$runner_uid:$runner_gid" "$owned"
done

echo "installed:"
sha256sum "$dest/accept-trusted.sh" "$dest/require-manual.sh"
echo "registered in $env_file:"
grep '^ACTIONS_RUNNER_HOOK_JOB_STARTED=' "$env_file"
echo
echo "The runner reads .env once, at service start, so the registration above"
echo "reaches a job only after a restart — and a restart kills a running job,"
echo "so do it while the runner is idle:"
systemctl list-units --plain --no-legend 'actions.runner.*.service' 2>/dev/null \
  | awk '{ print "  systemctl restart " $1 }'
