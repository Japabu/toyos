---
status: open
kind: defect
opened: 2026-08-09
---

# `pick_file` returns `None` for six reasons and the caller is told one of them

`userland/filepicker-api/src/lib.rs:15`. `pick_file -> Option<String>` answers
`None` when the user cancelled, when nothing is listening on `filepicker` yet,
when the send was refused, when the header read was refused, when the reply is
not `MSG_FILEPICKER_RESULT`, and when the path is not UTF-8. Its doc comment
claimed only the first.

The reachable one is a boot race, and it is the same class as
[`../kernel/terminal-races-compositor-at-boot.md`](../kernel/terminal-races-compositor-at-boot.md):
the compositor spawns `/bin/filepicker` at
`userland/compositor/src/session.rs:220` *after* printing `compositor: ready`,
so an editor that reaches Cmd+O before the picker has called `listen` is told
the user changed their mind. Unlike the terminal race this one is silent —
nothing exits, nothing logs, and the desktop looks correct.

The send is worse than the rest: `conn.send_bytes(...).ok()` discards the
refusal outright, so a failed request is followed by a blocking wait for a reply
to a message that was never delivered.

The fix is a signature that can say which — `Result<Option<String>, PickError>`,
where `Ok(None)` is the cancel — and it belongs with whatever the owner rules on
`specs/terminal-boot-race-options.md`, because "nothing is listening yet" is the
case that document exists to decide. Fixing the type without that ruling only
moves the guess to the caller.
