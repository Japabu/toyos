---
status: none
kind: rejected
opened: 2026-08-01
---

# The two TPDF dither draws share one `Xorshift32` state, and nothing can tell

Kept rather than deleted: the measurement is the finding, and an entry removed
silently gets re-filed next year by the next person who reads `rng.next() +
rng.next()` on one state and assumes.

Measured over two million samples, one state stepped twice versus two
independent states:

| | variance (TPDF ideal 0.16667) | χ²/df vs triangular | lag-1 autocorrelation |
|---|---|---|---|
| one state, two draws | 0.16672 | 0.98 | −0.00048 |
| two independent states | 0.16652 | 0.63 | −0.00050 |

The joint distribution of the summand *pair* is where a deterministic
relationship would actually show, and it does not: χ²/df ≈ 1.00 with zero empty
cells at 32×32, 128×128 and 512×512, for both arrangements. The step function
decorrelates the two draws well enough that the pair is empirically
indistinguishable from two independent streams.

**Deliberately not "fixed anyway".** Changing the dither changes the captured wav
bit-for-bit, so it would perturb the audio gate to chase a defect nobody can
demonstrate. This project has been bitten specifically by gates that cannot fail
(`specs/assessments/metal-track-history.md`); spending the gate's sensitivity on a
non-defect is the same error wearing a tidier hat.
