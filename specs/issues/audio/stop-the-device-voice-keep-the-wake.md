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
needs gate A's thorough tier on a quiet tree. That tier is itself red on `main`
(`thorough-tier-reds-on-unmodified-main`), so the instrument comes first.

That unblock condition is the useful part and the reason this is filed apart from
the fork-blocked cluster: it could land *first* if the quiet tree arrives before
fork access does.
