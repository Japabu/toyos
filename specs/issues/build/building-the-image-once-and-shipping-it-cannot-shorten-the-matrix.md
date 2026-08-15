---
status: none
kind: rejected
opened: 2026-08-15
---

# Building the boot image once and shipping it to the shards cannot shorten the matrix

Twelve shards each build the same tree before their first verdict, which reads
like twelve times the work — and it is, in runner minutes. It is not twelve
times the *wall clock*, because the twelve build concurrently and a job that
built once for all of them would sit on the same clock they do. Measured, so
nobody spends the day re-deriving it.

**The floor, run `31896922288`** (`main` at `e064a96`, twelve KVM shards),
means over the twelve unless a range is given:

| phase | s |
|---|---|
| `deps` (apt into `debian:sid`) | 52–59 |
| checkout + rustup + `install-toolchain.sh` | 14–20 |
| `actions/cache/restore` | 11–22 |
| **job setup, summed** | **83.9** (80–99) |
| `suite` step to `running N tests` — host crates, 110 C tests, `toyos-ld`, `toyos-cc`, 103 Rust test binaries | **76.4** (72.8–78.6) |
| `suite` step to the first `PASS` — the above plus the shipping image and its guest | **111.7** (105.2–119.2) |

So a shard's first verdict lands at about **196 s** into its job, and 112 s of
that is a build every one of the twelve performs identically.

**The arithmetic that declines it.** A dedicated builder job pays the same
83.9 s of setup and the same ~112 s of build, so the artifact cannot exist
before **T+196 s** — which is exactly when a shard that built it itself already
has it. `needs:` on such a job would idle every shard for its whole duration;
polling for the artifact instead (the shape `toolchain-ready` uses) reaches the
same instant, plus an upload and a download. `cache-writer` measures the
builder: its whole job, one fast test included, is **213 s**.

Nor can the builder start earlier. Everything it compiles needs the toolchain,
and `toolchain-ready` is what gates the matrix in the first place.

Nor does it help the shards that pay *more* than the floor: shipping
`metalcase`'s and `sshdcase`'s images too would put 198 s and 145 s of build
(`specs/issues/build/the-shard-split-prices-a-boot-and-not-the-image-behind-it.md`)
in series inside one job, past 500 s, against the 347 s widest shard it was
meant to shorten.

**What the idea would buy is runner minutes** — about 11 × 112 s ≈ 1,230 s per
run — and `ci.yml` already records why that is not the currency: the repository
is public and its minutes are unmetered. The queue is the thing minutes buy,
and §12.5 of `specs/assessments/ci-plan-assessment-2026-08.md` already spent
the large one there.

**What is still on the floor and is not this.** The 52–59 s `deps` step is a
package install repeated in every guest job on every run, and it is not a build
at all: `specs/issues/build/every-guest-job-installs-its-own-packages.md`.

Rejected on measurement, 2026-08-15, by the CI wall-clock task that was sent to
build it.
