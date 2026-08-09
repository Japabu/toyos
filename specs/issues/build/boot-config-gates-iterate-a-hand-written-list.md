---
status: open
kind: finding
opened: 2026-08-09
---

# The sshd boot gate iterates a hand-written list of boot modes, not the boot modes

`no_shipped_boot_config_starts_sshd` (`src/build.rs:1095`, the file's only
`#[test]`) is the gate behind CLAUDE.md's *"sshd is built into the image and
started by nobody"*. It reads:

```rust
for boot in [Boot::Normal, Boot::Diag, Boot::Console] {
```

`Boot` has exactly those three variants today (`src/build.rs:478-491`), so the
gate is complete — by coincidence of maintenance rather than by construction. A
fourth boot mode is one variant and a `config()` arm; nothing makes its author
add a row here, and nothing goes red if they do not. The failure is silent in the
direction that matters: a new shipped image that starts sshd from init passes.

Two smaller things in the same shape:

- The **eight test configs are deliberately outside** it — `tests/metalcase` and
  `tests/sshdcase` both start sshd on purpose — and that is right, but it is
  right only because the test's *name* says `shipped_boot_config`. There is no
  assertion that the three named variants are the shipped ones and the other
  eight are not; the split lives in the reader's head.
- `find . -name system.toml -not -path './rust/*' -not -path './target/*'`
  answers 11. Nothing compares that answer to any list in `src/build.rs`, so a
  twelfth config is covered by no gate at all.

The fix is one list. A `const ALL_CONFIGS: &[(&str, Shipped)]` asserted equal to
what the walk finds, with the gate iterating the `Shipped` ones, turns both
silences into reds — and it is the natural place for any further per-config
gate to hang, of which `specs/capability-endowment-spec.md` §8.1 proposes five.
That spec's chunk 4 rewrites all eleven configs and is the cheapest moment to do
it; it is recorded here because it is worth doing whether or not that branch
lands.
