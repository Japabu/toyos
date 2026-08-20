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

# Cargo's executable remains in /opt/cargo. Only its downloaded registry and
# git data live on the runner cache mounted at /toyos-cache, while rustup's
# stable installation stays immutable in the image layer.
ENV CARGO_HOME=/toyos-cache/cargo \
    TOYOS_LOCAL_CACHE=/toyos-cache
