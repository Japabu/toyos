---
status: open
kind: defect
opened: 2026-08-07
---

# Gate A's thorough tier is red on `main`, and the recorded dropout sample is what it disagrees with

Measured 2026-08-07 on `main` at `c0365ea`, in one session, three arms:

| tree | dropout runs | measured runs | verdict |
|---|---|---|---|
| `main` | 7 | 28 | `pooled dropout rate: 7 of 40 vs recorded 0 of 120 (Fisher p=4.00e-5)` |
| `wt/toyos-m3` (M3) | 5 | 12 | `5 of 40 … (p=8.02e-4)` |
| M3 with its one new wait deleted | 5 | 40 | `5 of 40 … (p=8.02e-4)` |

The denominators differ because the gate stops as soon as the remaining runs
cannot change the verdict, so each arm ran until it was decided. **All three
fail, and `main` fails hardest.** The gate's own documentation says it cannot
detect a doubling of the dropout rate at any N a human waits for, so it also
cannot separate these three from each other — what it says unambiguously is that
every one of them is far from the recorded `0 of 120`.

Every gap is small and none is a silence anyone would hear as a break: the
largest is 51 periods, most are one or two, and the fast tier — whose verdict is
harm — is **green on all three arms**, 7 of 7 each. So this is a *rate* finding
against a recorded sample, not a report that the machine sounds wrong.

Two readings, and nothing here decides between them:

- the recorded sample in `tests/audio-baseline.toml` was taken with one QEMU at a
  time and no concurrent agents, and this host now runs several worktrees at once
  — `specs/audio-gate-history.md`'s own lesson is that these counters drift
  between batches on one host with no code change;
- or something landed since the sample was recorded and nobody re-ran the
  thorough tier to notice.

The second is testable and the first is not, so **the next step is the thorough
tier on the commit the sample was recorded against**, on this host, in one
session. Do not re-record the baseline first: a sample re-taken now would make
the disagreement disappear without anyone learning which of the two it was, and
the recorded zero is the only reason the question is visible at all.

Host load was 6–20 throughout and is **not** offered as the explanation — the
owner's 2026-08-04 ruling stands, and the three arms ran back to back on the same
host anyway, which is what makes them comparable to each other.

**Consequence for anything landing while this is open:** the thorough tier cannot
serve as a pass/fail gate. M3 used it as an A/B instead, which is what it could
still answer, and landed on the fast tier plus the full suite.
