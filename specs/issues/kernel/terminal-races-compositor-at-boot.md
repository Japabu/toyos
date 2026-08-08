---
status: open
kind: defect
opened: 2026-08-06
---

# `/bin/terminal` races the compositor at boot and exits, and the ready marker hides it

`init = ["/bin/compositor", "/bin/terminal"]` (`tests/desktopcase/system.toml`,
and every other desktop config). The kernel spawns both back to back and
`/bin/terminal` calls `Window::create_with_title` at once; if the compositor
has not reached its `listen` yet, `services::connect` refuses, the terminal
prints `terminal: no compositor is running` and exits 1, and the shell it has
already spawned exits behind it. The desktop then comes up with **no window at
all**.

Nothing notices, because `BootOptions::ready_marker` is `compositor: ready` and
the compositor is fine — it prints that line a few tens of milliseconds after
the terminal has gone. Every later assertion is then made against a desktop
with no terminal in it, and the message is whatever that test says about
typing: `nothing typed at the terminal window reached a shell`.

Twice in one session on a busy host, in different tests — `desktop_locale_detect`
in a 12-wide phase, and `desktop_window_child` in a landing gate whose suite
took 372 s against a quiet run's 135 s. The margin is small: terminal gone at
0.633 s, `compositor: ready` at ~0.7 s. A strong candidate for the
`desktop_locale_detect` half of `specs/issues/build/`'s `Sched::Parallel` red list.

Not fixed here, because *where* the wait belongs is a design question:
`window::Window::create` retrying would make every client wait for a
compositor that may legitimately be absent (`/bin/console` boots with none);
the terminal retrying puts the policy in one client; and sequencing `init` on a
service registration is kernel policy. What is not in doubt is that a client
which starts before its service is listening must not read that as "there is
no desktop".

**Measured 2026-08-08, and it is the dominant blocker of `specs/issues/build/`'s `Sched::Parallel`
red list rather than a candidate for one.** Eight full suites in one session,
two concurrent twelve-wide runs at a time on one host (`--host-slots 0`), four
on `main` and four on the branch that made `shell_echoes` say what it had
found:

| arm | suites | suites with the race in a boot log | red suites |
|---|---|---|---|
| `main` | 4 | 3 | 3 |
| branch | 4 | 1 | 1 |

**Every red suite in the session contained this race and every green one did
not.** The two other reds in the session were `audio_tone_load (smp=8)` and
`xhci_hid_break`, and each landed in a suite that already had one. All three on
`main` reported it as `nothing typed at the terminal window reached a shell` —
once as `desktop_typing_damage`, twice as `desktop_audio_client` — and each was
`ALONE: GREEN`, which is how a defect that reproduces in roughly half of these
suites has been read as host noise. `shell_echoes` now ends that wait on
`exit: terminal ` as well as on `terminal: ready` and names the race, so the red
arrives in about a second instead of holding a lane: 305 s in the run that
produced this table, with `desktop_window_child` beside it at 285 s.
