---
status: open
kind: finding
opened: 2026-08-20
---

# The kernel's clippy gate lints one feature set, and it is not the one the harness boots

`.github/workflows/host-tests.yml` runs

```
(cd kernel && cargo clippy --target x86_64-unknown-none -- -D warnings)
```

which builds the kernel with **default features only**. Every guest the harness
boots is built with `boot-actuators`, and several configs add `test-actuators`
— so all the code behind those two `cfg`s is linted by nothing, on a gate whose
whole claim is that default clippy is denied on every pull request (#132).

Measured 2026-08-20 on `wt/toyos-p2conv`, which had changed none of the files
below:

```
cd kernel && cargo clippy --target x86_64-unknown-none \
    --features boot-actuators,test-actuators -- -D warnings
```

**6 findings, in 4 files:**

| lint | site |
|---|---|
| `unusual_byte_groupings` | a hex literal |
| `manual_is_multiple_of` | |
| `type_complexity` | |
| `never_loop` ×2 | `src/log/storm.rs:148` (`loop { park_forever() }`) |
| `needless_range_loop` | `src/heartbeat.rs:254` (`for cpu in 0..cpus` indexing `stamps`) |

None of them is a bug — `storm.rs`'s is a `!`-returning call inside a `loop`,
which is the shape that says "this never returns" — but that is the point: the
gate cannot tell, because it never sees them. A real finding in actuator code
would land the same way.

The fix is one more invocation in the same step, not a new job. What it costs is
one more kernel check (2.5 s locally against a warm target). What is not obvious
is which feature *set*: `boot-actuators,test-actuators` is the union the harness
uses, but `kernel/Cargo.toml` declares fifteen more that no kernel build may
enable (`loom`, `wake-fence-off`, the six other model controls, `sleeplock-acquire-off`),
and `cargo test --lib` already closes that set. So the union of the two
actuator features is the whole of what is missing.

Found while landing pipeline 2's wall 3; not fixed there, because the six
findings are in four files that chunk does not touch.
