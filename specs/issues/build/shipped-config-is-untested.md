---
status: open
kind: defect
opened: 2026-08-07
---

# No test boots the config the project ships

`system.toml` is what `cargo run` builds and what a stick is flashed with, and
the harness boots none of it: `tests/testcases`, `desktopcase`,
`desktopaudiocase`, `doomcase` and `metalcase` are each their own config, and
`screen_diag_boot` / `screen_console_shell` boot `diag/` and `console/`. So the
shipping image's init list, its `hosted-rustc` setting and its program list are
exercised only by the owner running `cargo run`, which agents are told not to
do. The one gate on that file is `no_shipped_boot_config_starts_sshd`, which
reads it rather than booting it.

Noticed 2026-08-07 while landing `hosted-rustc = false`: that change alters only
`system.toml`, so no suite test could go red for it in either direction.
