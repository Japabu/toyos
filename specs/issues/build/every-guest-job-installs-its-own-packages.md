---
status: open
kind: finding
opened: 2026-08-15
---

# Every guest job spends about a minute installing its own packages

`ci.yml`'s `guest`, `tcg` and `cache-writer` each open a bare `debian:sid`
container and `apt-get install git curl ca-certificates jq zstd xz-utils
build-essential qemu-system-x86` before anything else can run — the image ships
none of them, and `actions/checkout` needs git.

Measured on run `31896922288` (`main` at `e064a96`), the `deps` step of the
twelve shards: **52, 53, 53, 53, 54, 54, 54, 54, 56, 56, 56, 59 s**, plus 52 s
in `cache-writer`. It is on the critical path of every job, on every run, and it
is the largest single item in the 83.9 s of setup a shard pays before its
`suite` step starts.

Nothing about it is a build, and nothing about it is the tree: the same eight
packages at the same versions, three times a day and thirteen jobs a run. What
would remove it is an image that already has them — built from a `Dockerfile` in
this repository, published once per change to it, pinned by digest the way
`.github/qemu-version` pins the instrument. **The pin is the whole point**: the
QEMU version a guest verdict is read against is declared once and
`.github/instrument.sh` reds on a disagreement, so an image carrying a
*different* QEMU would have to red the same way — an image is only allowed to be
faster than `apt`, never to change what was measured.

Not attempted here: it is a registry, a publish step and a second thing to keep
current, and the CI wall-clock task of 2026-08-15 that measured it was landing
the shard partition. The number is written down so the next person deciding
whether it is worth that machinery has one.
