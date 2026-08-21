# T14 CI appliance

Trusted Linux jobs use the image in this directory through a registry bound to
`127.0.0.1` on the T14. `.github/workflows/route.yml` names the registry
digest — once, for the whole repository — so a rebuild cannot silently move the
QEMU/Rust instrument. Fork pull requests and merge queues select `debian:sid`
on GitHub-hosted runners.

Only the jobs that need the accelerator, the persistent build cache or hours of
this machine's CPU are routed here: the guest lanes, the audio gate, the green
probe and the toolchain bootstrap. Every coordinating job stays GitHub-hosted,
where it is free and parallel — this runner has one worker, and a job waiting
there for something another job on the same machine has to produce cannot
finish.

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

`link-build-cache.sh` links every Cargo target before the suite. If the build
system invalidates an external-dependency stamp, Cargo replaces that link with
a real directory; `save-build-cache.sh` snapshots only those directories back
to the content-keyed cache before Actions cleans the checkout. It uses hard
links when the workspace and cache share a filesystem and falls back to a
normal copy when they do not.

## The job-start hook

`accept-trusted.sh` is what decides which GitHub event may run a job on this
machine, and a persistent runner attached to a public repository has nothing
else between a fork's branch and a laptop that keeps state between jobs. It
refuses any repository but `ToyOSOrg/ToyOS`; on `pull_request` it reads the
event payload and refuses a head outside the repository; on `schedule` and
`workflow_dispatch` it refuses a workflow that is not `main`'s; and it refuses
every other event type by name. `require-manual.sh` beside it is the narrower
predecessor — dispatches from `main` and nothing else — kept so the machine can
be locked down without editing a file on it.

Both are installed by an operator, as root, because the runner account has no
sudo:

```sh
sudo sh .github/runner/install-hook.sh            # or: require-manual
```

Read back what is actually running, from any checkout that can reach the
machine — the point of this section is that the answer is a diff and not a
belief:

```sh
ssh t14 cat /usr/local/libexec/toyos-runner/accept-trusted.sh \
  | diff -u .github/runner/accept-trusted.sh -
ssh t14 sha256sum /usr/local/libexec/toyos-runner/accept-trusted.sh \
                  /usr/local/libexec/toyos-runner/require-manual.sh
ssh t14 grep ACTIONS_RUNNER_HOOK_JOB_STARTED /home/t14/actions-runner/.env
```

A non-empty diff means the machine is running a revision that is not this one:
either the operator step has not been run since the file changed, or somebody
edited the machine. Neither is a state to leave.

## What a trusted job can do to this machine

The hook admits the job; after that the machine is the job's. GitHub's runner
mounts `/var/run/docker.sock` into every container job — its own doing, plainly
visible in the `docker create` line in `~/actions-runner/_diag/Worker_*.log`,
and no workflow setting turns it off — so an admitted job can start a
privileged container and own the host. **The container's user decides who owns
the files a job writes, not whether a job could take the laptop.** The boundary
is `accept-trusted.sh` and the set of people who can trigger a trusted event,
and there is no second one.

That is why the jobs run unprivileged even though it buys no isolation. The
image carries `USER ci` at the runner's own uid (`Dockerfile`), so a job owns
the work area and the cache entries it writes. The alternative was the shape
this replaced: root jobs leaving trees the runner account could not clean, and
a root `ubuntu:24.04` container started by the hook on *every* admission to
chown them back — a per-job privileged container in the one file that must be
readable at a glance, paying for nothing the socket had not already given away.

Measured on the machine, in the image the workflows name
(`docker run --device=/dev/kvm`, no `--user`, no `--privileged`):

```
uid=1000(ci) gid=1000(ci) groups=1000(ci),994(kvm)
KVM_GET_API_VERSION=12
KVM_CREATE_VM fd=4
```

`build-image.sh` re-asserts the first two lines of that on every rebuild.

## Rebuild the image

From a trusted checkout on the T14:

```sh
sh .github/runner/build-image.sh
```

The script reads the host's `kvm` gid, starts the loopback registry if
necessary, verifies QEMU, Rust and the unprivileged shape, pushes the tag, and
prints the image reference. Copy the printed digest into
`.github/workflows/route.yml`, which is the repository's one reference to it —
every workflow that names an image reads it from there. Never replace that
digest with a moving tag.

A machine whose `/dev/kvm` carries a different gid needs its own build: the gid
is baked into the image because that is the only place Docker will apply a
supplementary group from. Getting it wrong does not fail loudly by itself, so
`.github/instrument.sh` opens `/dev/kvm` when the node is present and reds if
it cannot — the alternative is a suite that quietly emulates.

## Inspect storage

```sh
docker system df
du -sh /home/t14/actions-runner-cache/*
find /home/t14/actions-runner-cache/build -mindepth 1 -maxdepth 1 \
  -type d -printf '%TY-%Tm-%Td %p\n' | sort
```

Old build or toolchain keys can be deleted individually while the runner is
idle. Keep the key named by the current `main` tree so the next run stays warm.
