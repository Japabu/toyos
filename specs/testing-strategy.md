# Testing strategy

## 1. Invariants

1. **Every defect class has exactly one owning instrument.** The owner's red is
   the class's alarm. A class no instrument can observe is listed in §7.
2. **A pull-request verdict is deterministic for its author.** Green means the
   merged result is clean. Red means the diff is defective. A gate that reds at
   a rate independent of the diff is itself the defect, and is fixed or
   removed.

## 2. Instruments and ownership

| instrument | owns | blind to |
|---|---|---|
| host suites | pure logic: decoders, validators, layouts, the build system's own gates | everything that requires a booted kernel |
| KVM guest shards | the booted kernel on native silicon: memory, processes, syscalls, IPC, filesystems, drivers against QEMU's device models | contention; semantics the hypervisor absorbs |
| TCG shard | ISA breadth: instruction paths the KVM hosts' CPUs never decode | vendor-real semantics; realistic instruction cost |
| loaded dev host | contention: parallel suites, lock storms, scheduler collapse | vendor semantics; run-to-run comparability |
| kernel-loom | memory orderings x86 TSO hides from every guest test | code outside the modeled primitives |
| gate A | audible harm | everything inaudible |
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

Required checks: `host`, `abi-split`, `gate-stage`, `guest-suite`.

- `host` runs every host suite.
- `guest-suite` aggregates the KVM shards, which run exactly the Fast tier,
  and the `durations` verdict.
- The shards are a partition: every Fast test runs exactly once per run, and
  the merge refuses duplicates, gaps, and partial shard sets.
- Exactly one job per run writes the shared build cache; it runs on every
  pull-request run.
- The boot image is built once per run. A dependency is never rebuilt because
  a timestamp moved.

A pull-request red that is not about the author's diff is adjudicated in
`src/redlist.rs` and fixed at its owner; it is never re-run away.

## 5. The nightly

The scheduled run executes what the pull-request gate withholds: the Nightly
tier on the same KVM shard configuration, the TCG shard, gate A's thorough
tier, and stress.

- It runs once nightly and on manual dispatch; never on push.
- A red updates the single standing alarm issue. The alarm is not the record:
  every nightly red is adjudicated the same day into a fix, a
  `src/redlist.rs` row, or a tier correction.
- A nightly red standing unadjudicated for three days is a process defect and
  takes priority over feature work.
- Nightly measurements refresh the recorded Nightly costs; they are validated
  against the tier rule, never against equality with a past measurement.

## 6. The local suite

The dev host's suite is developer feedback and the only contention
instrument. It is never a gate: nothing merges on the strength of a local
green, and no local red blocks a merge — it is adjudicated like any other
instrument's finding.

## 7. The metal checklist

Defect classes only silicon can observe. An entry names a measurement, not a
topic; names what closes it; and does not replace an automated tripwire: the
tripwire catches recurrence, the entry prices the consequence.

| # | measurement | closes |
|---|---|---|
| 1 | one T14 boot with `no-ap-control-regs` armed against one without, same image, same session; record the delta | `specs/issues/kernel/ap-control-registers-inherit-init.md` |

Every metal session walks this table before anything else.

## 8. Substrate

- GitHub-hosted runners only.
- The pull-request gate's wall clock is bounded by its slowest required job.
  Setup cost is attacked before coverage is: a setup cut needs a measurement,
  a coverage cut needs an invariant-level justification.

Evidence and derivations, frozen at their date:
`specs/testing-strategy-assessment-2026-08-12.md`.
