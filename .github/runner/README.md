# T14 CI appliance

Trusted Linux jobs use the image in this directory through a registry bound to
`127.0.0.1` on the T14. The workflow names the registry digest, so a rebuild
cannot silently move the QEMU/Rust instrument. Fork pull requests and merge
queues still select `debian:sid` on GitHub-hosted runners, and
`portability.yml` deliberately keeps its from-scratch bootstrap.

The image contains the apt dependencies and stable Rust. The host directory
`/home/t14/actions-runner-cache` is mounted only into trusted T14 containers:

- `toolchains/<tag>` holds the extracted content-addressed ToyOS compiler;
- `build/guest-<tag>-<lock hash>` is linked directly as each Cargo target;
- `cargo` holds Cargo registry and git downloads;
- `host-target` is the persistent target for host-side Linux helper jobs.

There is one Actions runner and one worker, so no two jobs can mutate these
caches concurrently. A cancelled build can leave an incomplete Cargo target;
Cargo resumes or rebuilds it. An incomplete compiler entry has no `.complete`
marker and is emptied before the next download.

## Rebuild the image

From a trusted checkout on the T14:

```sh
sh .github/runner/build-image.sh
```

The script starts the loopback registry if necessary, verifies QEMU and Rust,
pushes the tag, and prints the image reference. Copy the printed digest into
the T14 image references in `ci.yml`, `gate-a.yml`, and `probe-green.yml`.
Never replace those digest references with a moving tag.

## Inspect storage

```sh
docker system df
du -sh /home/t14/actions-runner-cache/*
find /home/t14/actions-runner-cache/build -mindepth 1 -maxdepth 1 \
  -type d -printf '%TY-%Tm-%Td %p\n' | sort
```

Old build or toolchain keys can be deleted individually while the runner is
idle. Keep the key named by the current `main` tree so the next run stays warm.
