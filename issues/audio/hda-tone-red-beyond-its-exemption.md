---
status: open
kind: defect
opened: 2026-08-07
task: 88
---

# `hda_tone` is red on `main` for a reason `#88`'s exemption does not cover

`cargo test -- hda_tone` on `main` at `6d11938`, alone, 2026-08-07 18:5x:

```
FAIL hda_tone: 1 mid-tone silences in the capture: total 1 [1p×1]
  FAIL  hda_tone  (15s)  — listed against #88, and this is not that failure:
        the entry covers ["the captured tone is not one sine"]
```

The `EXPECTED_FAILURES` entry does what it is supposed to: it pins the assertion
rather than the test, so a *second* defect in the same test still reds the run
and says which. What is red is the mid-tone-silence assertion — a gap in the
capture, which is gate A's harm verdict — and not #88's spectral one. So any
landing whose gate is `cargo test` is currently red on `main` for this, and an
agent will read it as theirs.

Found while landing task #98/#12: the same test failed identically inside that
landing's gate, and the A/B against `main` in the same session is what
identified it as `main`'s. Assigning it needs whoever owns H3 —
`5fdfeb7`/`a022811` ("wip: H3, the virtio-sound stub and its userland driver")
landed hours before this measurement.
