---
status: open
kind: defect
opened: 2026-08-15
---

# `i8042_keyboard` can measure 11 s against a 4.9 s price, and the `durations` gate reds on it

The test passed. What went red is the required gate behind it:

```
the merged CI profile and tier declaration disagree:
i8042_keyboard measured 11070 ms in CI, over the 10000 ms line, but
i8042_keyboard remains Fast
```

PR #85, run `31903757592`, `durations` job `95059153793`, the measurement from
shard 2's `durations-shard-2` artifact. The shard itself was green — `8 passed,
8 total (105.5 s)`, with `PASS i8042_keyboard (11s)` in its log — and the
committed price is **4,924 ms**.

**Not a slow runner, or not only one.** Shard 2's other seven names against
`tests/test-durations`:

| name | measured | committed | × |
|---|---|---|---|
| `i8042_no_spurious_wake` | 247 | 231 | 1.07 |
| `virtio_net_no_msix` | 2,172 | 2,011 | 1.08 |
| `i8042_undecoded_bytes` | 5,708 | 5,116 | 1.12 |
| `desktop_locale_detect` | 4,445 | 3,966 | 1.12 |
| `console_line_atomicity` | 5,492 | 4,791 | 1.15 |
| `screen_recoverable_untouched` | 5,532 | 4,551 | 1.22 |
| `i8042_mouse` | 4,360 | 3,572 | 1.22 |
| **`i8042_keyboard`** | **11,070** | **4,924** | **2.25** |

So the job was 7–22% slow and this one name was 125% slow. About six seconds
went somewhere the others did not.

**The shape that fits, and it is a hypothesis and not a measurement.**
`tests/toyos-rust-tests/src/bin/i8042_keyboard.rs` — the one guest binary eight
harness callers drive — exits on a sentinel (the End key's release) and keeps
`Instant::now() + Duration::from_secs(5)` only as "a liveness ceiling, not the
measurement … this only bounds a run that lost it". A run that loses the
sentinel therefore pays five seconds it does not normally pay, which is the
size of the gap. `specs/assessments/test-cost-audit.md` records that the fixed
deadline *was* the whole family's cost history until the sentinel replaced it,
and that the fallback was kept deliberately. Nothing in the job log says the
sentinel was lost; the verdict does not depend on it, so the test passes either
way and only the clock notices.

**It is not `main`'s standing state**, so nobody should read this as a tier that
was always wrong: `main`'s own pushes `31901526246` and `31902837318`, an hour
before, both passed `durations`.

The strategy leaves two honest answers and neither is a tolerance band
(`specs/testing-strategy.md` §3: "There is no tolerance band"; §1.2: a gate that
reds at a rate independent of the diff is itself the defect):

- **Find the six seconds.** If it is the lost sentinel, then either the host's
  injection or the guest's read can drop it, and that is a defect in a delivery
  path a keyboard test exists to gate — the fallback is doing its job by hiding
  it from the verdict and showing it on the clock.
- **Price the fallback.** A test whose worst case is its own liveness ceiling
  plus a boot cannot be a 4.9 s Fast test unless that ceiling is under the
  line; the ceiling is 5 s, the boot is ~5 s, and 10 s is exactly the line.

Filed by the documentation batch whose pull request it reddened, which changed
no code that can reach it.
