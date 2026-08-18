---
status: assigned
kind: defect
opened: 2026-08-01
---

# The cpal ToyOS backend hardcodes 44100/2ch/i16 and rejects everything else

soundd's resampler and channel-conversion paths (`specs/audio-subsystem-spec.md`
§6 and §8) are unreachable from any real client and therefore effectively
untested. The backend also `assert_eq!`s the device rate against a compile-time
constant, so changing the driver's rate aborts every cpal app.

Deferred to the quiet-tree window, not neglected: editing that fork needs
`.cargo/config.toml` path overrides, which redirect cpal for **every** agent in
the tree. Same scheduling constraint as the fork lint audit
(`specs/plans/fork-lint-audit-plan.md`).

**Client liveness is blocked on this, not on soundd.** The ambiguity between a
paused and a wedged client is *specified*: `specs/audio-subsystem-spec.md` §6.4
defines pause as "no explicit coordination required", and the cpal backend's
`pause()` is a purely local futex store soundd is never told about. No change
confined to soundd can separate the two, and landing the soundd and SDK halves
alone would kill every paused cpal client. This is a case where the **spec**,
not the implementation, is what needs to change.
