---
status: open
kind: track
opened: 2026-08-19
---

# Strict required checks price every queued branch a full re-run, and a merge queue would not

`gate-stage` requires `strict_required_status_checks_policy: true`
(`.github/workflows/landing.yml`), which is what makes a passing check mean the
*merged* result passed and not just the branch head. The cost of that
guarantee, on this repository: a branch queued behind a landing must re-run its
full required suite — a toolchain bootstrap and the required `guest-suite`
check, which is itself gated on twelve `guest (1..12)` shards in `ci.yml` —
against `main`'s new tip before it can merge in turn. Several branches landing
back-to-back each pay that in full, once per landing they queue behind.

That cost is what motivated a same-day experiment on 2026-08-19: turning
`strict_required_status_checks_policy` off. `gh run list --workflow
landing.yml` shows what happened instead of the intended relief — `gate-stage`
refused every landing across seven worktrees for the whole window (roughly
13:41 to 14:28 UTC), sealing the repository rather than letting anything merge
against a stale base. That is exactly the property `gate-stage`'s own comment
claims for strict (*"Strict is what makes a check on this head a check on the
merged result — the property `--land`'s `git merge --no-ff main` used to
carry"*), so the setting was restored the same day (ruleset `20589156`'s
`updated_at` is `2026-08-19T16:28:15+02:00`, i.e. `14:28:15Z`, matching the next
landing run's success at `14:28:56Z`).

The throughput concern that prompted the experiment is real and still open;
weakening `strict` is not the fix — the fix, if the repository wants one, is
GitHub's merge queue: it serializes queued pull requests against a speculative
merge of each with `main`'s current tip and runs required checks once per
queued merge rather than once per branch per intervening landing, without
giving up the guarantee `strict` provides.

Open at the implementation pass: whether `gate-stage` needs a check for the
merge-queue ruleset clause once one exists, how the twelve-`guest`-shard matrix
behaves costed against a speculative merge commit rather than a real PR head,
and whether `abi-split`'s base-branch fetch still resolves correctly under a
queue's temporary merge ref.
