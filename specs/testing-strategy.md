# Testing strategy

## 1. Invariants

1. **Every defect class has exactly one owning instrument.** The owner's red is
   the class's alarm. A class with no owning instrument is recorded as
   unowned in `specs/issues/`.
2. **A pull-request verdict is deterministic for its author.** Green means the
   merged result is clean. Red means the diff is defective. A gate that reds at
   a rate independent of the diff is itself the defect, and is fixed or
   removed.

## 2. Instruments and ownership

| instrument | owns | blind to |
|---|---|---|
| host suites | pure logic: decoders, validators, layouts, the build system's own gates; the memory orderings x86 TSO hides (kernel-loom) | everything that requires a booted kernel |
| KVM guest shards | the booted kernel on native silicon: memory, processes, syscalls, IPC, filesystems, drivers against QEMU's device models; audible harm (gate A) | contention; semantics the hypervisor absorbs |
| TCG shard | ISA breadth: instruction paths the KVM hosts' CPUs never decode | vendor-real semantics; realistic instruction cost |
| metal | consequences emulation absorbs: cache and control-register effects, PAT/MTRR, device timing, real latency | anything requiring repetition or isolation; it is one manual machine |

A defect found by a non-owning instrument transfers to its owner: the owner
gains a test, or the class gains a §7 entry.

## 3. Tiers

A guest test is **Fast** iff its verdict and its duration are invariant under
machine speed: an exit code, an expected output, a decoded structure. A test
whose verdict or duration is anchored to real time — it plays or records in
real time, waits out a staged window, measures a rate; a twice-as-slow machine
changes its verdict or its price — is **Nightly**.

- The Fast ceiling is **10,000 ms, measured on the PR instrument**. There is
  no tolerance band. A Fast test measuring over the ceiling reds the required
  gate.
- Tier movement is by measurement, in both directions. A relegation records
  the measured cost and the coverage the PR gate loses. A relegated test whose
  fresh measurement is at or under the ceiling returns to Fast.
- The tier declaration and the measured profile must agree; the required
  `durations` verdict enforces the agreement on every pull request.

## 4. The pull-request gate

Required checks: `host`, `abi-split`, `gate-stage`, `guest-suite`, `build`.

- `host` runs every host suite.
- `guest-suite` aggregates the KVM shards, which run exactly the Fast tier,
  and the `durations` verdict.
- The shards are a partition: every Fast test runs exactly once per run, and
  the merge refuses duplicates, gaps, and partial shard sets.
- Exactly one job per run writes the shared build cache; it runs on every
  pull-request run.
- The boot image is built once per run. A dependency is never rebuilt because
  a timestamp moved.
- `build` builds and publishes the toolchain every guest shard installs.

A pull-request red that is not about the author's diff is adjudicated in
`src/redlist.rs` and fixed at its owner; it is never re-run away.

## 5. The nightly

The scheduled run executes what the pull-request gate withholds: the Nightly
tier on the same KVM shard configuration, the TCG shard, and gate A's
thorough tier.

- It runs once nightly and on manual dispatch; never on push.
- A red updates the single standing alarm issue. The alarm is not the record:
  every nightly red is adjudicated the same day into a fix, a
  `src/redlist.rs` row, or a tier correction.
- A nightly red standing unadjudicated for three days is a process defect and
  takes priority over feature work.
- A nightly run uploads the measured profile; the recorded Nightly costs are
  refreshed from it by commit and validated against the tier rule, never
  against equality with a past measurement.

## 6. The local suite

The dev host's suite is developer feedback. It is never a gate: nothing
merges on the strength of a local green, and no local red blocks a merge.

## 7. The metal checklist

A defect class only silicon can observe carries an entry on the metal session
checklist. An entry names a measurement, not a topic; names what closes it;
and does not replace an automated tripwire: the tripwire catches recurrence,
the entry prices the consequence. A metal session walks the checklist before
anything else.

## 8. Substrate

- GitHub-hosted runners only.
- The pull-request gate's wall clock is bounded by its slowest required job.
  Setup cost is attacked before coverage is: a setup cut needs a measurement,
  a coverage cut needs an invariant-level justification.

## 9. CI mechanics

- **The instrument is declared, not read off the image.** `.github/qemu-version`
  names the one QEMU version every guest — CI's and the dev host's — is
  measured against. Every gate workflow that boots a guest runs
  `.github/instrument.sh` after checkout, which prints the version, the host
  CPU model and whether `/dev/kvm` is present, and reds on a version
  disagreement; `cargo test --lib` (`src/ci.rs`) refuses a gate workflow that
  installs QEMU and never runs it. The dev host reads the same declaration and
  answers a disagreement with a `Note:`, never a refusal — its QEMU moves on
  its own schedule and a build must not stop for that.
- **A shard writes only its own name.** The Fast tier's shard partition (§4)
  is measured against `tests/test-durations`, the profile a machine with
  nothing measured starts from. A shard writes
  `test-durations.shard-<i>-of-<n>` and never the committed file itself;
  `cargo run -- --merge-durations`, run once per CI run over the whole shard
  set, is the only writer of `tests/test-durations`, and it refuses a name two
  shards both measured as well as a set missing a shard.
- **Publishing the toolchain is idempotent.** The release tag is the hash of
  the four trees that compile into the sysroot — `rust`, `toyos-abi/src`,
  `toyos/src`, `userland/libc/src` — so two branches carrying the same four
  trees both build the same tag safely, and being second is not an error. A
  job that needs the toolchain waits for the *asset* to be attached, not for
  the release tag to answer: `gh release create` makes the tag before the
  upload finishes, so a tag that exists is not evidence the asset does.
