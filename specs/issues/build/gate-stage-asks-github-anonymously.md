---
status: open
kind: defect
opened: 2026-08-14
---

# gate-stage asks the GitHub API anonymously, so a shared runner IP can 403 it

The `gate-stage` job's branch-protection step reads `rules/branches/main`
with an unauthenticated `curl`, and its comment reasons that asking nothing
of the workflow token means the step "cannot start failing because a
permission narrowed". Unauthenticated requests are rate-limited per source
IP, and a hosted runner's IP is shared across tenants: PR #51's run got
`curl: (22) The requested URL returned error: 403` in four seconds, and a
required check went red on a diff it never read.

The token the job already holds reads the same endpoint within its
`metadata: read` grant, and authenticating moves the rate limit from the
runner's shared IP to this run. That keeps the property the comment is
actually after — the step asks for nothing beyond what every workflow token
carries.
