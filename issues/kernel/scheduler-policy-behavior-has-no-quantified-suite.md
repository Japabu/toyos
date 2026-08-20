---
status: open
kind: track
opened: 2026-08-20
---

# Scheduler policy behavior has no quantified suite

From the external review of 2026-08-20: the model checking and SMP stress
methodology verify functional correctness; the POLICY — "threads execute,
processes own fair share" — is encoded in implementation intent and validated
nowhere empirically. The suite this track owes, each case with a measured
bound rather than a pass/fail vibe:

- extreme thread-count asymmetry between processes (the pathological case
  named outright: a process attempting to gain share by creating many
  runnable threads — the policy's whole claim is that it cannot);
- mixed interactive/background workloads;
- migration under load;
- wakeup storms;
- starvation bounds (a runnable task's worst-case wait, measured);
- nested parkability/budget narrowing (the `Operation` machinery under
  composition);
- fairness under CPU contention, as numbers.

The pure `toyos-sched` crate and its simulator are the natural home — policy
cases are host-testable there without a guest boot, which is what makes
measured bounds affordable per-PR where they are cheap and nightly where
they are not.
