---
status: open
kind: defect
opened: 2026-08-10
---

# Gate A's CI workflow runs QEMU 8.2.2, and every other guest in CI runs 11.0.3

`.github/workflows/gate-a.yml` runs on a bare `ubuntu-24.04` and installs QEMU
from apt, which is **8.2.2**. `.github/workflows/ci.yml`'s twelve shards and its
`tcg` canary run in a `debian:sid` container for **11.0.3**, which is the dev
host's version, and `specs/ci-plan.md` §7.3 is the measurement that put them
there: on one runner image, one commit and one accelerator, `desktop_typing_damage`
is red on 8.2.2 and green on 11.0.3, and `usb_storage_shapes` with it.

That makes gate A's runner arm a third instrument. `specs/ci-plan.md` §9's
standing open item is whether a runner's gate A spread is comparable with the
dev host's — the recorded sample in `tests/audio-baseline.toml` is the dev host's
on QEMU 11.0.3, and §7.3 says the shards are on 11.0.3 *so that* CI and the dev
host differ in the accelerator and nothing else. A number taken on 8.2.2 answers
a question nobody asked, and §9 says the thorough tier may not move until that
number exists.

Nothing here says the audio path is version-sensitive; virtio-sound and HDA are
not the QMP injection path §7.3 caught. What it says is that the one arm whose
whole purpose is a comparison is not comparable, and that this is invisible —
the workflow reads perfectly well and never prints what it is comparing against.

**Not fixed on the CI task of 2026-08-10 for one reason: it cannot be verified
cheaply.** `gate-a.yml` is `workflow_dispatch` only, its shortest useful run is
hours, and containerising it also means adding a `rustup-init` step the ubuntu
image gives for free — so the change is small and the confidence in it would
come from nothing but reading. The shards' `deps`, `rust` and `install the
toolchain` steps are the worked example to copy.

The other half is that the job should print the version it ran, the way every
KVM job prints its `model name` (§2). An instrument that does not say what it is
cannot be told apart from the one it is being compared with.
