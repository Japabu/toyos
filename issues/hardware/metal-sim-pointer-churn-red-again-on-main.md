---
status: open
kind: defect
opened: 2026-08-11
---

# `metal_sim_pointer_churn` is red on `main`'s CI again, after two write-ups retired it

`issues/hardware/xhci-flap-wedges-under-kvm.md` records it twice: red alone
in run `31247206462` with `bound 0 pointer sources`, then **0 of 5** in the rate
probe (`31258202923`, tree `f8f73e1`) and **closed** — "a console the test had
counted before it caught up". `issues/hardware/eleven-names-red-on-ci.md`
carries the same 0 of 5 as one of the twelve that came off the list, and
`issues/build/parallel-tests-red-under-other-suites.md` has a third,
dev-host observation of it that says "observed once … not investigated".

None of those is what run `31396171916` says. `main` at `7af7c20`, 2026-08-10,
shard 2, read with `gh run view --log-failed` on 2026-08-11:

```
FAIL metal_sim_pointer_churn: [qemu] Init process crashed during boot:
  FAIL  metal_sim_pointer_churn  (244s)
  ALONE metal_sim_pointer_churn: GREEN, and it was alone both times — nothing the
        harness controls differed, so it failed once and passed once. That is a
        rate and not a classification.
```

Three things separate it from everything already written down.

- **The message is a boot crash, not a count.** `bound 0 pointer sources` is the
  test's own verdict about what it enumerated; `[qemu] Init process crashed
  during boot:` is the harness reporting that the guest never got as far as the
  test. Whatever the console-ordering fix closed, it was not this.
- **244 s in the phase on a machine with one guest on it.** CI is `--jobs 1`,
  one guest per runner, so the dev host's contention reading is unavailable here
  by construction.
- **The harness says the classifier cannot help.** `ALONE: GREEN` on a shard is
  two runs of the same width, so the line prints the rate reading rather than
  the contention one, and there is no rate: this is one sample.

Not diagnosed. Filed so that the next agent who greps this name does not find
three documents that all say 0 of 5 or closed and stop there. The queryable form
is `cargo run -- --known-red metal_sim_pointer_churn`, which prints this row
beside the two that retired it.

What would settle it is `probe-green.yml`'s shape aimed at this one name — ten
reps, one job per rep — since that is what turned each of the four names behind
four consecutive red runs on `main` from a run into a number.
