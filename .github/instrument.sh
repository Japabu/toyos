#!/bin/sh
# What this job is about to measure with, named before it measures anything.
# Three variables a verdict from this job must be read against
# (specs/ci-plan.md §2, §7, §7.3): the QEMU version against
# `.github/qemu-version` — a disagreement reds, since `debian:sid` is a
# rolling release and the remedy is to record the new version, not to carry
# on; the host CPU vendor, since `kvm_amd` and `kvm_intel` are both in play
# and not selectable; and whether `/dev/kvm` is there, the only difference
# between a `guest` shard and the `tcg` canary.
#
# Run from the repository root, after the checkout, by every job that boots a
# guest.
set -eu

here=$(dirname "$0")

want=$(grep -v '^#' "$here/qemu-version" | tr -d '[:space:]')
first=$(qemu-system-x86_64 --version | head -1)
have=$(echo "$first" | sed -n '1s/^QEMU emulator version \([^ ]*\).*/\1/p')

echo "$first"
if [ -f /dev/kvm ] || [ -c /dev/kvm ]; then
  accel=$(ls -l /dev/kvm)
else
  accel="no /dev/kvm node: this is the emulated arm"
fi
echo "$accel"
cpu=""
[ -r /proc/cpuinfo ] && cpu=$(sed -n 's/^model name[^:]*: //p' /proc/cpuinfo | head -1)
cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo '?')
echo "cpu: ${cpu:-unknown}, $cores core(s)"

if [ "$have" != "$want" ]; then
  echo "::error::this job runs QEMU '${have:-$first}' and .github/qemu-version declares $want."
  echo "::error::specs/ci-plan.md 7.3: the QEMU version decides test outcomes, so a number"
  echo "::error::taken on one is not a number about the other, and the dev host's baseline is"
  echo "::error::recorded on $want. debian:sid is a rolling release and this is what it"
  echo "::error::moving looks like — nothing here is about the tree."
  echo "::error::The remedy is one line: put the new version in .github/qemu-version, in a"
  echo "::error::commit that says the instrument changed."
  exit 1
fi

echo "- \`${GITHUB_JOB:-job}\` ${MATRIX_LABEL:-}: QEMU $have, ${cpu:-unknown CPU}, $cores core(s)" \
  >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
