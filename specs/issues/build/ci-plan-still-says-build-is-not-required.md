---
status: open
kind: finding
opened: 2026-08-13
---

# `ci-plan.md` still recommends what GitHub already has

`specs/ci-plan.md` §11 (the paragraph beginning "`build` is still not a
required check"): "It is now refused *by construction* through
`guest-suite`, so the accident is gone, but the check whose name says 'the
toolchain did not build' is still advisory... **Recommended.**" GitHub's live
ruleset for `main` (`GET /repos/Japabu/toyos/rules/branches/main`, read
2026-08-13) already lists `build` under `required_status_checks` alongside
`host`, `abi-split`, `gate-stage` and `guest-suite` — the recommendation was
acted on. `specs/testing-strategy.md` §4 and `landing.yml`'s `gate-stage`
declaration now name `build` too, so the law and the ruleset agree; this
evidence document is the one place still describing `build` as advisory.
