# Input: raw in the kernel, translated at the surface

The kernel used to decide what a key *types*. It held the layout tables, the
dead-key state and a terminal's escape vocabulary, and delivered a struct that
carried both the physical key and up to five bytes of its opinion about it.

That is now userland's, one instance per surface, and the kernel delivers the
transition and nothing else.

## 1. What the kernel keeps, and why

`RawKeyEvent` is two bytes: `keycode` (HID usage) and `modifiers` (the mask,
plus `MOD_RELEASED` for the direction).

Three things stayed, and each earns it:

- **The multi-keyboard merge and its held-set.** `HELD` is a bitmap over HID
  usages for the whole machine, and `handle_key` refuses a transition to the
  state a usage is already in — which is what makes a PS/2 typematic repeat and
  an unchanged USB report behave identically. Device arbitration is mechanism.
- **`modifiers`, derived from that held-set.** This is the one judgement call
  in the split, and it went to the kernel for a reason that survives the move:
  the mask is the union across *every* keyboard, so Shift held on one and a
  letter typed on another makes a capital. A surface that reconstructed the
  mask from the transitions it saw would not have seen the ones that arrived
  while another surface had the focus — a window that gains focus between
  Shift-down and the letter has a hole no protocol closes cheaply. One byte in
  the event deletes that whole class of resync.
- **Ctrl+Alt+D.** On HID usage 0x07, so it is the same three physical keys
  under every layout, and it is *recorded* rather than run because every caller
  of `handle_key` holds its driver's guard.

`release_all` and the io_uring wake plumbing are unchanged.

**Both queues are bounded.** `keyboard::MAX_QUEUED_EVENTS` and
`mouse::MAX_QUEUED_EVENTS` are 512, drop-oldest: what a queue nobody is
draining is *for* is the most recent input, not the first 512 events after the
reader stopped. `device::try_claim` discards whatever is queued when the device
changes hands, so one program never receives another's keystrokes.

## 2. `toyos-keymap::Translator`

The whole of what a surface does with a press: the layout table, the dead-key
machine over it, the control codes Ctrl makes of the letter row, and the escape
sequences for the keys no layout defines. One instance per surface.

Per-surface is deliberately *more* correct than the kernel's one-per-machine
composer: a pending diacritic belongs to the thing being typed into, and a `^`
typed at one terminal must not compose with the `e` typed at another. The host
test `two_translators_do_not_share_a_pending_diacritic` is that property.

`MAX_EMIT` is still 5 and the compile-time walk still proves nothing can exceed
it — extended to cover the escape table, which is the third producer.

The crate no longer has a kernel caller and is `#![no_std]`, `forbid(unsafe_code)`,
37 host tests.

## 3. The surface tree

A **surface** is something with a screen and a keyboard. Its owner reads
transitions from whatever is above it and decides what they mean.

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
`surface.<pid>` and puts that name in `TOYOS_SURFACE`, which a child inherits —
so a wizard three processes below a terminal finds it without anything being
passed down by hand.

Five message types, all of them bare headers except one:

| | direction | payload |
|---|---|---|
| `MSG_GRAB_KEYS` | client → host | — |
| `MSG_GRAB_GRANTED` / `MSG_GRAB_REFUSED` | host → client | — |
| `MSG_KEY` | host → client | `RawKeyEvent` |
| `MSG_LAYOUT_CHANGED` | either | — |

The grant is not optional: a client that assumed it had the keys would wait
forever for events going somewhere else. While a grab is held the surface stops
translating — and, importantly, stops *advancing* its translator, which is why
`Window::press` is the only way to get characters and is called by whoever
wants them rather than on every event. A `^` the wizard consumed must not still
be pending when the wizard exits.

`MAX_CLIENTS` is 4. Past it a connection is accepted and closed, which the
client sees as `GrabError::HostGone` — never a queue, because a client waiting
behind three others for keys the user is pressing now is worse off than one
told no.

The server side is non-blocking throughout: `ipc::FrameRx` (lifted out of the
compositor, which now uses it) buffers a frame whole before anything acts on
it, and every host→client write is one `try_send`. A *short* write drops the
peer by name; a syscall error is a peer that has gone, and is not a fault.

## 4. Layout is a file

`toyos::surface::LAYOUT_CONFIG` — `/home/root/.config/keyboard_layout`. Written
by `locale <name>` and `locale detect` through the same `set()`, read by every
translator when it starts.

**The notification carries no name.** `MSG_LAYOUT_CHANGED` says only that the
file moved; each translator re-reads it. Nothing can end up holding an opinion
that disagrees with the file, and there is no authority to arbitrate between
two surfaces.

It travels up to the root and back down: a child tells its terminal, the
terminal tells the compositor over the window protocol, and the compositor
broadcasts to every window. The compositor translates nothing, so it does not
act on it — it exists in that path only so every window gets the same answer to
a question one of them changed.

`/home` is tmpfs on the T14, so the choice survives a login and not a reboot.

## 5. What this deleted

- `SYS_SET_KEYBOARD_LAYOUT` (23) and its handler, the ABI wrapper, and
  `toyos::system::set_keyboard_layout`.
- `RawKeyEvent::translated` and `::len` — 6 of its 8 bytes.
- `kernel/src/keyboard.rs`'s `translate`, `ACTIVE_LAYOUT`, `COMPOSER`,
  `set_layout`, `layout_name`, and the kernel's `toyos-keymap` dependency.
- `locale --load` and the `system.toml` init line that ran it.
- The compositor's private `ClientRx`/`RxStep`/`fill` (moved to the SDK).

## 6. Gates

- `swiss_german_layout` — 25 key positions injected through the i8042, asserted
  as the exact string `zyüöäà@€[<>\êÊÜ^é^q§`, now observed at a surface's
  translator rather than at the kernel's. The same sequence is a host test in
  `toyos-keymap/tests/translate.rs`, so the tables are gated in milliseconds
  and the *pipeline* is gated by the boot.
- `i8042_keyboard` — the `us` gate, unchanged assertions.
- `locale_detect` / `locale_detect_unrecognized` — the wizard under a host that
  holds the keyboard, which is the state that used to make it refuse.
- `console_locale_detect`, `desktop_locale_detect` — the wizard under the two
  surfaces the machine actually has. Both end by typing the key a US board
  prints `[` on and asserting `ü`, which only a re-read of the config the
  wizard just wrote can produce.
- `input_merge` — the kernel-internal merge, now asserting exactly
  `(0x04, MOD_SHIFT)` on the letter rather than the capital it used to produce.
