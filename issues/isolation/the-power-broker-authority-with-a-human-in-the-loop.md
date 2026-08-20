---
status: open
kind: track
opened: 2026-08-20
---

# The power broker: authority with a human in the loop

Opened from the owner's question about Linux's interactive shutdown
permission, the day `SYS_SHUTDOWN` gained `Rights::POWER`. Linux answers
"the person at the keyboard said so" with three glued-on systems (a
privileged service, a policy engine, an authentication agent) because its
base model only knows identities. ToyOS's capability model IS the mechanism,
so the native shape is smaller:

1. **No user-facing program holds `POWER`.** One small daemon — the power
   broker — is endowed it in `system.toml`, and its whole job is deciding.
   (Today the toybox shutdown applet holds the bit directly; the broker
   moves that single endowment one level up, behind judgment.)
2. **Programs request, the broker decides** — a port/connection like every
   other daemon protocol in the tree, under the server-never-blocks
   doctrine.
3. **The confirmation rides the trusted UI path.** The broker asks the
   compositor to present it, and the tree's own architecture is what makes
   that mean something: the compositor owns the panel and the kernel
   delivers key transitions per surface, so no ordinary program can draw a
   fake dialog or fake the click on a real one. "A human physically present
   confirmed" is the single-user machine's honest equivalent of Linux's
   password prompt.
4. **Inhibitors**: a program may register "unsaved work" with the broker;
   the broker delays, or names the holdouts in the dialog. Registration is
   a connection, so a crashed registrant releases its inhibit by the same
   teardown that releases everything else.

Unstaffed until the owner opens it; sequenced naturally with the userland/
product era. What must not happen meanwhile is the accident this track
exists to prevent: `POWER` spreading to more manifest rows because asking
the applet is inconvenient — the broker is the answer to that itch.
