---
status: open
kind: finding
opened: 2026-08-09
---

# `--abi-split-check` refuses a branch for a change its tree does not contain

`src/pr.rs:421` sets `touches_sysroot` by walking each commit's name-status
output, so the verdict is a property of the branch's *history* rather than of
what it would merge. A branch that edited `toyos-abi/src` and then reverted it
is refused, though `git diff origin/main...HEAD` names no sysroot source at all.

Hit on 2026-08-09: a one-line doc-comment correction to `SYS_CONNECT` was taken
back off a docs branch, and the check still refused the branch afterwards. The
workaround is to build the branch again from `origin/main` — history cannot be
rewritten here, so there is no cheaper one.

The rationale the refusal prints is about *holding the shared sysroot from the
branch's first build until it lands*, and that rationale is about the tree an
agent builds, not about what an intermediate commit once said. Nobody but the
author builds an intermediate commit; CI and the merge build the tip.

Not obviously worth fixing as stated — a diff-based check would stop refusing
the reverted case, but it would also stop refusing a branch that lands an ABI
change and a caller change as one commit, which is the case the rule exists for.
The honest fix is probably both: refuse on the net diff, and keep the per-commit
walk only for the `Abi-Inseparable` trailer it already reads there.
