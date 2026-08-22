---
status: open
kind: defect
opened: 2026-08-21
---

# The T14 runner is trusted, not isolated

A job admitted to the self-hosted runner can take the machine, and the cache it
leaves behind is what the next job builds against. `.github/runner/README.md`
states both where an operator will read them; this file is the standing entry
that says they are still true.

**Root, unavoidably.** GitHub's runner mounts `/var/run/docker.sock` into every
container job — the `docker create` line in `~/actions-runner/_diag/Worker_*.log`
shows it, and no workflow setting turns it off — so an admitted job can start a
privileged container and own the host. Running the trusted image unprivileged
(`USER ci`, uid 1000) removed the per-job root cleanup container and the
root-owned leftovers that made it necessary; it bought no isolation and is not
claimed to. `.github/runner/accept-trusted.sh` is the entire boundary.

**The cache is trusted state.** A job can write anything into
`/home/t14/actions-runner-cache`, and the next job with the same content key
links those bytes in as its Cargo target directory. A content key is about
reuse, not integrity, and nothing detects a planted artifact. Exit: an
ephemeral runner, or a cache whose entries are signed by the run that produced
them. Neither exists.

**The fork pull-request policy is the owner's to raise.**
`gh api repos/ToyOSOrg/ToyOS/actions/permissions/fork-pr-contributor-approval`
reports `first_time_contributors`, so a fork pull request from anyone with one
landed commit runs workflows unapproved. Those runs are GitHub-hosted and the
hook refuses them regardless, so this is not a hole in the T14 — it is what
stands between a stranger and a second attempt at finding one.
`all_outside_collaborators` is the recommendation and it is a settings change
no file here can make.

The accounts that can trigger a trusted run are the two that can push at all
(`Japabu` admin, `stu214634` write), which is why the above is accepted rather
than blocking. It stops being acceptable the moment a third party gets write
access.

**2026-08-22 narrowed what reaches the machine, and changed none of the
above.** `route.yml` now sends every `pull_request` and every `push` to
GitHub-hosted runners, so in the ordinary course only a `schedule` other than
`ci.yml`'s and a `workflow_dispatch` arrive here — a smaller surface, reached
by fewer events, but by the same two accounts and with the same consequences
once admitted. `accept-trusted.sh` is unchanged and still admits a same-repo
`pull_request` and a `push` by name: it is the machine's boundary, not a copy
of the routing rule, and it is what holds if a workflow names the `toyos`
labels directly or the routing regresses. The exits named above — an ephemeral
runner, a signed cache, `all_outside_collaborators` — are exactly as owed as
they were.
