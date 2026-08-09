---
status: open
kind: defect
opened: 2026-08-08
---

# `screen_pager_keys` is red on `main`, and no keystroke reaches the halted pager

Found 2026-08-08 by the licence work's gate, and it belongs to nobody yet. It
is what currently stops any branch landing on the default gate.

```
FAIL screen_pager_keys: 0 page moves over 30 keystrokes in 0.4s — an
unattended deadline alone could have produced 1.1 of them, so nothing here
says a keystroke reached the halted pager
```

| Tree | Where | Result |
|---|---|---|
| `wt/toyos-licence` | full suite, its serial tail | FAIL, 0/30 over 0.4 s |
| `wt/toyos-licence` | alone | FAIL, 0/30 over 0.3 s |
| `b36cf64` — the branch point, i.e. `main` — same session, same host, changes stashed | alone | FAIL, 0/30 over 0.3 s |

The rest of that suite was green: 291 passed, 1 failed, 292 total. So the
branch is exonerated and the defect is on `main`.

What the test asserts is not decoration. It is **the only place** the claim
"PageUp/PageDown reach a machine that has stopped scheduling" is made at all:
`toyos-ps2`'s decode is host-tested, but that a keystroke crosses the i8042
into a halted CPU's poll is a fact about the controller and the poll. Its own
comment says so. On the T14 that path is how a photographed panic report is
steered to the page somebody needs, and CLAUDE.md advertises it.

Zero of thirty says the poll never sees the byte — not that it sees it late.
Note the earlier phases pass: the footer appears, and the *unattended* 3 s
deadline still advances the page, so the pager is running and painting. Only
the keyboard half is dead.

**Bisected, and the answer is a merge whose two parents are both green.**
`8273964` (2026-08-07 23:40) is the last landing whose gate was the whole suite
and its recorded line is `test result: ok. 291 passed, 291 total (366.2s)`, so
the window was one day. Seven boots in one session, `cargo test --
screen_pager_keys` at each:

| Commit | | Result |
|---|---|---|
| `543c7b0` | before both lanes | **PASS** 18.6 s |
| `9bd7a9e` | `idt: a vector without a gate is not a fault` — main's lane, no dump work | **PASS** 18.4 s |
| `f7c87ee` | `dump: the panel gets the report, and keeps it` — the dump lane, no IDT work | **PASS** 17.9 s |
| `3dfb216` | the third lane (harness/CI), off `543c7b0` | **PASS** 17.9 s |
| **`f96d52e`** | **`wt/toyos-dump: merged main db34b2b` — the merge of the two above** | **FAIL** 10.0 s |
| `eaaac80` | docs only, on top of it | **FAIL** 8.4 s |
| `1bcbc99` | `log_ring: a mark is a store under the lock` | **FAIL** 10.2 s |
| `b36cf64` | main's tip at the branch point | **FAIL** ×3, 6–7 s |

**`f96d52e` is the first bad commit and neither parent is bad.** Everything
between `f7c87ee` and `f96d52e` on main's side is `9bd7a9e` (green alone),
`a0f724c` (touches only `fault_gates.rs` and one harness line) and three
docs-only commits, so the regression is not any single change: it is the
interaction of the panic console's new `hold_report` repaint with the IDT
work that landed beside it. Both agents' gates were honest and both were green.

That is the class this codebase has no gate for — CLAUDE.md's landing rule is
that main's tip must *compile*, and this is a case where main's tip stopped
*working* while every branch that built it passed.

`hold_report` remains the first thing to look at: it repaints the report from
`drain_irqs` whenever 128 remembered pixels say the panel stopped carrying it,
and a repaint that restores the page a keystroke just moved is
indistinguishable, to this test, from a keystroke that never arrived — the
footer is its only instrument. What the bisect adds is that the repaint alone
does not do it.

**Two things it also settles.** Host load is *not* the cause: the landing gate
that produced the fifth red ran at load 2.1–2.4 with `fastest boot 1388 ms
against the reference 1320 ms`, i.e. a quiet host at 1.05x, and the failure was
byte-identical to the ones taken at load 11–16. And every green took 18 s
against every red's 6–10 s, which is the pager phases taking real time versus
not happening at all.

**A separate instrument weakness, worth fixing whoever wins.** The sampling
loop sends one key and screendumps immediately: `footer()` returns on its first
dump when a footer is present and only sleeps 50 ms when there is none. Thirty
samples in 0.3 s is a 10 ms round trip, so a guest slower than that to repaint
reads as "did not move" on every sample. The bisect says that is not what is
happening here — a timing margin would not split this cleanly by commit — but a
verdict with no margin at all belongs with
`specs/issues/build/parallel-tests-red-under-other-suites.md` and the rest of
that class.

It is **not** in `EXPECTED_FAILURES` and must not be put there to get a landing
through: an entry needs a task, a write-up and the failure text it covers, and
declaring another agent's regression expected is how a real red becomes
permanent.
