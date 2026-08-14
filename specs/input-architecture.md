# Input

The kernel delivers key transitions and nothing else. A raw key event is two
bytes: the key's HID usage and the modifier mask, with one bit for the
direction. Translation — layout tables, dead keys, control codes, escape
sequences — happens in userland, one translator per surface.

## 1. The kernel side

- **The multi-keyboard merge.** The kernel holds one held-set over HID usages
  for the whole machine and refuses a transition to the state a usage is
  already in, which makes a PS/2 typematic repeat and an unchanged USB report
  behave identically.
- **The modifier mask is derived from that held-set**, so it is the union
  across every keyboard: Shift held on one keyboard and a letter typed on
  another makes a capital. A surface cannot reconstruct the mask from the
  transitions it sees — transitions delivered while another surface held the
  focus never reach it — so the mask rides in the event.
- **Ctrl+Alt+D** is matched on HID usages, the same three physical keys under
  every layout, and is recorded rather than acted on inside the delivery
  path.

The keyboard and mouse queues hold 512 events each, dropping oldest, so an
undrained queue holds the most recent input. Whatever is queued is discarded
when the device changes hands, so one program never receives another's
keystrokes.

## 2. The translator

Everything a surface does with a press: the layout table, the dead-key
machine over it, the control codes Ctrl makes of the letter row, and the
escape sequences for keys no layout defines. One instance per surface: a
pending diacritic belongs to the thing being typed into, and a `^` typed at
one terminal must not compose with an `e` typed at another.

No press emits more than five bytes, over every layout, dead-key and escape
table.

## 3. The surface tree

A **surface** is a screen-and-keyboard pair. Its owner reads transitions
from the layer above it and decides what they mean.

```
        compositor            claims the keyboard; translates nothing
            │  key input (window protocol)
            ▼
        terminal              holds this process's translator
            │  surface channel (MSG_KEY)
            ▼
        shell → child programs
```

The console is the same tree with the console at the root: it claims the
keyboard itself, owns the translator, and serves the same channel.

### The channel

The host of a surface serves a per-process channel name, which children
inherit through the environment, so a process any depth below a terminal
finds the channel without explicit hand-off.

Five message types, all bare headers except one:

| | direction | payload |
|---|---|---|
| `MSG_GRAB_KEYS` | client → host | — |
| `MSG_GRAB_GRANTED` / `MSG_GRAB_REFUSED` | host → client | — |
| `MSG_KEY` | host → client | one raw key event |
| `MSG_LAYOUT_CHANGED` | either | — |

A client acts only after `MSG_GRAB_GRANTED`; until the grant, events are
delivered elsewhere. While a grab is held the surface neither translates nor
advances its translator, so a dead key the grab-holder consumed is not
pending when it exits.

A surface serves at most 4 clients. Past that a connection is accepted and
closed, which the client observes as the host being gone; there is no wait
queue.

The host side never blocks on a client: it buffers a frame whole before
acting on it, and every host-to-client write either completes at once or
drops that peer. A short write drops the peer by name; a failed write is a
peer that has gone, not a fault.

## 4. Layout configuration

The layout choice is one file, `/home/root/.config/keyboard_layout`, written
by the locale tools and read by every translator when it starts.

`MSG_LAYOUT_CHANGED` carries no layout name: it says only that the file
changed, and each translator re-reads the file, so no translator can hold an
opinion that disagrees with it and no authority arbitrates between surfaces.

The notification travels to the root and back down: a child tells its
terminal, the terminal tells the compositor, and the compositor broadcasts
to every window. The compositor translates nothing and does not act on the
message itself.

`/home` is tmpfs on machines without writable storage there, so the choice
survives a login and not a reboot.
