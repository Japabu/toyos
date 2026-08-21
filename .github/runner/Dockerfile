FROM debian:sid

ARG DEBIAN_FRONTEND=noninteractive
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        jq \
        qemu-system-x86 \
        xz-utils \
        zstd \
        > /dev/null \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable \
    && qemu-system-x86_64 --version | head -1 | grep -F 'version 11.1.0' \
    && rustc -vV \
    && apt-get clean \
    && find /var/lib/apt/lists -mindepth 1 -delete

# The job runs as the runner's own uid, not as root. A container job writes
# into the runner's work area through a bind mount, so a root job leaves
# root-owned trees the runner user cannot clean or overwrite — which is what
# the job-start hook used to repair with a second, root, container per job.
# Owning what it writes removes the repair and the container with it.
#
# `KVM_GID` is the gid `/dev/kvm` carries on the runner host, read from
# `getent group kvm` by `build-image.sh`. It is a build argument rather than a
# constant because it is a fact about one machine: an image built where that
# gid differs and hardcoded here would open no accelerator, and the guest suite
# would fall back to emulation instead of saying so. `USER ci` (a name, not a
# uid:gid pair) is what makes Docker apply the supplementary group at all.
#
# `/opt/rustup` moves with it: `rustup toolchain link toyos` writes the linked
# toolchain into `RUSTUP_HOME`, and an unprivileged job cannot write a
# root-owned one.
ARG KVM_GID
RUN groupadd --gid "${KVM_GID:?the runner host's kvm gid is required}" kvm \
    && useradd --uid 1000 --user-group --create-home --shell /bin/bash ci \
    && usermod --append --groups kvm ci \
    && chown -R ci:ci /opt/rustup /opt/cargo

# Cargo's executable remains in /opt/cargo. Only its downloaded registry and
# git data live on the runner cache mounted at /toyos-cache, while rustup's
# stable installation stays immutable in the image layer.
#
# `TOYOS_LOCAL_CACHE` is also the one thing a job asks to learn where it is
# running: every workflow keys its trusted/hosted steps on this variable
# naming a writable directory, never on the runner's name.
ENV CARGO_HOME=/toyos-cache/cargo \
    TOYOS_LOCAL_CACHE=/toyos-cache
USER ci
