---
status: open
kind: defect
opened: 2026-08-21
---

# `ci.yml`'s `merge health` step goes green on a `--merge-health` that died

`.github/workflows/ci.yml`'s `merge health` step is the only `| tee` pipeline in
`.github/` that neither sets `pipefail` nor is deliberately statusless:

```yaml
      - name: merge health
        run: |
          cargo run -- --merge-health 2>&1 | tee /tmp/merge-health.log
          ...
          if grep -q 'THRESHOLD BREACHED' /tmp/merge-health.log; then
```

The runner logs `shell: sh -e {0}` for every step of these container jobs. In a
pipeline the status is the *last* command's, so `tee`'s 0 is the pipeline's 0 and
`-e` never sees `cargo run`'s. If `--merge-health` panics, refuses, or cannot
reach the API, the step appends an empty fenced block to the job summary, finds
no `THRESHOLD BREACHED` in an empty file, and reports success.

**What that costs.** The owner's 2026-08-20 easing of the merge law is explicitly
a measured trade — "its instrument and threshold live in the tracker, and past
the threshold the stronger serialization returns"
(`issues/build/the-eased-merge-law-carries-a-threshold.md`). This step is that
instrument's only reader in CI. An instrument that reports "not breached" when it
did not run is worse than one that is absent, because the absence is visible.

**The fix is one line**, `set -o pipefail` ahead of the pipeline — the idiom the
sibling `merge durations` step in the same file already uses, and the same fix
`.github/workflows/gate-a.yml` took on 2026-08-21. Do not reach for a status file
instead: under `-e` the left side of the pipe dies before it can record `$?`,
which was measured, and the step then goes green on a red for a second reason.

Two neighbours are *not* this defect and should stay as they are.
`probe-green.yml`'s `named tests` step sets `set +e` and ends `exit 0` on
purpose — "a red is the datum, not the job's verdict". `merge durations` already
sets `pipefail`.

Found while fixing `gate-a.yml`'s exit code; filed rather than fixed, because a
CI status change belongs to a run that can demonstrate it and this one could not.
