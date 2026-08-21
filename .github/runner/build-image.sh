#!/bin/sh
# Build the T14-only CI image and publish it to the registry bound to the
# runner's loopback interface. GitHub Actions always pulls a job container;
# the local registry makes that pull a local layer check rather than a WAN
# transfer. Run this from a trusted checkout on the T14 whenever the image tag
# below changes.
set -eu

root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
image=127.0.0.1:5000/toyos-ci:t14-20260821-2

# The accelerator's group on *this* host. The image puts its unprivileged user
# in it; a wrong number here is an image whose jobs silently emulate, so it is
# read rather than written down.
kvm_gid=$(getent group kvm | cut -d: -f3)
test -n "$kvm_gid"

if ! docker container inspect toyos-registry >/dev/null 2>&1; then
  docker run -d \
    --name toyos-registry \
    --restart always \
    --publish 127.0.0.1:5000:5000 \
    --volume toyos-registry-data:/var/lib/registry \
    registry:3 >/dev/null
fi

if [ "$(docker inspect -f '{{.State.Running}}' toyos-registry)" != true ]; then
  docker start toyos-registry >/dev/null
fi

docker build --pull --build-arg "KVM_GID=$kvm_gid" \
  --file "$root/.github/runner/Dockerfile" --tag "$image" "$root"
docker run --rm "$image" sh -c \
  'qemu-system-x86_64 --version | head -1 | grep -F "QEMU emulator version 11.1.0"; cargo -V; rustc -vV'

# The unprivileged shape, asserted rather than assumed: uid 1000 with the
# accelerator's group, and the accelerator open to it. `--device=/dev/kvm`
# alone is what the workflows pass, so this is the exact question they ask.
docker run --rm --device=/dev/kvm "$image" sh -c '
  set -eu
  id
  test "$(id -u)" = 1000
  test -w /dev/kvm
  echo "the image runs unprivileged and opens /dev/kvm"'

docker push "$image"
docker image inspect --format '{{range .RepoDigests}}{{println .}}{{end}}' "$image"
