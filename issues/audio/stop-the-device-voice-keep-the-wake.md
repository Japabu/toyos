---
status: open
kind: defect
opened: 2026-08-01
---

# An idle soundd keeps the DMA engine and the codec voice open

Stopping the device voice while keeping the periodic timer wake recovers the DMA
engine and the codec — the battery-relevant hardware — and gives up only the wake
itself. Resume still works unchanged, because soundd keeps writing signal bytes,
so it does not need the missing client→soundd message that
`cpal-backend-hardcodes-the-format` is waiting on.

**It is blocked on the audio gate, not on the fork and not on the owner.** A
mid-session device stop/restart is an audible transient plus a DLL re-lock, which
needs gate A's thorough tier on a quiet tree. That tier reds on the dev host
(`thorough-tier-reds-on-unmodified-main`), so the instrument comes first.

2026-08-21: the sentence above used to read "that tier is itself red on `main`",
which was being sourced partly from the CI nightly — and the nightly's red was an
exit-code defect over a printed verdict, not a verdict. The dev-host red it now
names is the one that was ever measured, and the block stands on it. The same
entry records why the runner's PASSes do not lift it.

That unblock condition is the useful part and the reason this is filed apart from
the fork-blocked cluster: it could land *first* if the quiet tree arrives before
fork access does.
