# Input

The kernel delivers key transitions and nothing else. `RawKeyEvent` is two
bytes: `keycode` (HID usage) and `modifiers` (the modifier mask, plus
`MOD_RELEASED` for the direction). Translation — layout tables, dead keys,
control codes, escape sequences — happens in userland, one
`toyos-keymap::Translator` per surface.

## 1. The kernel side

- **The multi-keyboard merge and its held-set.** `HELD` is a bitmap over HID
  usages for the whole machine; `handle_key` refuses a transition to the state
  a usage is already in, which makes a PS/2 typematic repeat and an unchanged
  USB report behave identically.
- **`modifiers`, derived from that held-set.** The mask is the union across
  every keyboard: Shift held on one keyboard and a letter typed on another
  makes a capital. A surface cannot reconstruct the mask from the transitions
  it sees, because transitions delivered while another surface held the focus
  never reach it; the byte in the event closes that hole.
- **Ctrl+Alt+D**, matched on HID usage 0x07 — the same three physical keys
  under every layout — and recorded rather than run, because every caller of
  `handle_key` holds its driver's guard.

Both queues are bounded: `keyboard::MAX_QUEUED_EVENTS` and
`mouse::MAX_QUEUED_EVENTS` are 512, drop-oldest, so an undrained queue holds
the most recent input. `device::try_claim` discards whatever is queued when the
device changes hands, so one program never receives another's keystrokes.

`release_all` and the io_uring wake plumbing sit below this layer and are
layout-independent.

## 2. `toyos-keymap::Translator`

Everything a surface does with a press: the layout table, the dead-key machine
over it, the control codes Ctrl makes of the letter row, and the escape
sequences for keys no layout defines. One instance per surface: a pending
diacritic belongs to the thing being typed into, and a `^` typed at one
terminal must not compose with an `e` typed at another (host test
`two_translators_do_not_share_a_pending_diacritic`).

`MAX_EMIT` is 5, and a compile-time walk over all three producers — layout,
dead-key and escape tables — proves nothing can exceed it. The crate is
`#![no_std]`, `forbid(unsafe_code)`, host-tested; the kernel does not depend
on it.

## 3. The surface tree

A **surface** is a screen-and-keyboard pair. Its owner reads transitions from
the layer above it and decides what they mean.

```
        compositor            claims the keyboard; translates nothing
            │  MSG_KEY_INPUT (window protocol)
            ▼
        /bin/terminal         window::Window holds this process's Translator
            │  toyos::surface (MSG_KEY)
            ▼
        /bin/shell → /bin/toybox locale detect
```

`/bin/console` is the same tree with the console at the root: it claims the
keyboard itself, owns the translator, and serves the same channel.

### The channel

`toyos::surface`, in the SDK, no_std and no alloc. The host serves
`surface.<pid>` and puts that name in `TOYOS_SURFACE`, which children inherit,
so a process any depth below a terminal finds the channel without explicit
hand-off.

Five message types, all bare headers except one:

| | direction | payload |
|---|---|---|
| `MSG_GRAB_KEYS` | client → host | — |
| `MSG_GRAB_GRANTED` / `MSG_GRAB_REFUSED` | host → client | — |
| `MSG_KEY` | host → client | `RawKeyEvent` |
| `MSG_LAYOUT_CHANGED` | either | — |

A client acts only after `MSG_GRAB_GRANTED`; until the grant, events are
delivered elsewhere. While a grab is held the surface neither translates nor
advances its translator: `Window::press` is the only way to get characters and
is called by whoever wants them rather than on every event, so a `^` the
grab-holder consumed is not pending when it exits.

`MAX_CLIENTS` is 4. Past it a connection is accepted and closed, which the
client observes as `GrabError::HostGone`; there is no wait queue.

The server side is non-blocking throughout: `ipc::FrameRx` buffers a frame
whole before anything acts on it, and every host→client write is one
`try_send`. A short write drops the peer by name; a syscall error is a peer
that has gone, not a fault.

## 4. Layout configuration

`toyos::surface::LAYOUT_CONFIG` is `/home/root/.config/keyboard_layout`,
written by `locale <name>` and `locale detect` through the same `set()`, read
by every translator when it starts.

`MSG_LAYOUT_CHANGED` carries no layout name; it says only that the file
changed, and each translator re-reads the file. No translator can hold an
opinion that disagrees with the file, and no authority arbitrates between
surfaces.

The notification travels to the root and back down: a child tells its
terminal, the terminal tells the compositor over the window protocol, and the
compositor broadcasts to every window. The compositor translates nothing and
does not act on the message itself.

`/home` is tmpfs on the T14, so the choice survives a login and not a reboot.

## 5. Tests

- `swiss_german_layout` — 25 key positions injected through the i8042,
  asserted at a surface's translator as the exact string
  `zyüöäà@€[<>\êÊÜ^é^q§`. The same sequence is a host test in
  `toyos-keymap/tests/translate.rs`: the tables are checked in milliseconds,
  the pipeline by the boot.
- `i8042_keyboard` — the `us` layout.
- `locale_detect` / `locale_detect_unrecognized` — the wizard under a host
  that holds the keyboard grab.
- `console_locale_detect`, `desktop_locale_detect` — the wizard under the two
  shipping surfaces. Both end by typing the key a US board prints `[` on and
  asserting `ü`, which only a re-read of the config the wizard just wrote can
  produce.
- `input_merge` — the kernel-internal merge, asserting exactly
  `(0x04, MOD_SHIFT)` on the letter.
