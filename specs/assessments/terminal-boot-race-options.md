# Where the wait belongs: `/bin/terminal` and the compositor at boot

**Settled by the capability endowment branch, and it is option E in all but
name.** A port exists before either end's process does, so a client's first
frame is queued on it whether or not a server has been spawned: there is no
instant at which a lookup can fail and nothing to retry. The two retry loops
this file costs are deleted, `connect_before_serve` is the gate, and
`specs/capability-endowment-spec.md` is the design. The menu below is kept as
the record of what the alternatives cost — the defect it opens on
(`kernel/terminal-races-compositor-at-boot`) is closed and its file is gone.

## 0. The menu

| | option | ABI? | rung | if the server never comes | gate has teeth? |
|---|---|---|---|---|---|
| A | `Window::create` retries | no | checked (poll) | every window client pays the bound | needs a mutated tree |
| B | `/bin/terminal` retries | no | checked (poll) | terminal exits later | proves one binary |
| C | kernel `init` sequences | no | checked | init decides, declared in config | needs an actuator that does not exist |
| **D** | **`connect` gains a waiting form; caller states the bound** | **yes** | **checked, but the retry loop and the unbounded wait become unrepresentable** | **one block, one wake, caller's bound** | **yes — two arms, no mutated tree** |
| **E** | **the server starts its clients** | **no** | **unrepresentable, for the pairs it covers** | **the client is never started** | **yes — host `--lib` config refusal** |
| F | hand down a connection, not a name | no | unrepresentable, for the pairs it covers | the client is never started | yes — syscall histogram |

Recommendation in §4: **D with the bound at the call site, E's narrow form beside
it, F as the destination.** The rest of the file is why.

## 1. What actually happens

`init = ["/bin/compositor", "/bin/terminal"]`. The kernel spawns both from one
loop (`kernel/src/main.rs:604-610`, 7 lines) and waits for nothing.

Both processes then reach their first service call at almost the same instant.
Measured on this host, one green boot of `desktop_typing_damage`
(`cargo test --test toyos-build -- desktop_typing_damage --nocapture`, 2026-08-09,
green in 34.1 s):

| kernel time | line | what it means |
|---|---|---|
| 0.263 s | `spawned /bin/compositor pid=0` | |
| 0.269 s | `spawned /bin/terminal pid=1` | **6 ms** after the compositor |
| 0.304 s | `shm: 0xc0000000 mapped WriteCombining into pid 0` | inside `Session::start`, *after* its `listen` |
| 0.309 s | `spawn: /bin/shell pid=2` | the terminal's `Command::new`, *before* its `Window::create` |
| <0.414 s | `compositor: ready` | the ready marker every desktop test boots on |

So the compositor's `listen` landed at or before 0.304 s and the terminal's
`connect` after 0.309 s. **The margin is single-digit milliseconds out of ~40 ms
of process startup each**, and the two spawns are 6 ms apart. On the T14 the same
window is wider: `spawned /bin/compositor` at 1.216 s, first shm map at 1.388 s
(`specs/assessments/metal-logs/2026-08-08-audio-wake/2026-08-08-153139-clean.log`), so the
compositor's `listen` is within **172 ms** of its spawn and `compositor: ready`
within 313 ms.

Nothing can be moved earlier. `services::listen("compositor")` is the first
statement of `Session::start`, which is the first statement of the compositor's
`main` (`userland/compositor/src/session.rs:130`,
`userland/compositor/src/main.rs`). What precedes it is dynamic linking, TLS and
relocation.

### The API surface, as it is

- `sys_connect` (`kernel/src/arch/syscall.rs:1287-1331`, 45 lines) looks the name
  up, creates two pipes, installs the client's fd, queues the server's end and
  returns. **It never blocks.** The ABI's own doc comment says
  *"Connect to a named service. Blocks until the server accepts."*
  (`toyos-abi/src/syscall.rs:752-753`). That claim is false — a doc comment is a
  claim to verify, and this one does not survive reading the function it
  describes.
- A name that is not registered is `SyscallError::NotFound`, and there is no
  other answer. `CreateError::NoCompositor` admits the conflation in its own doc
  (`userland/window/src/lib.rs:80-82`), and is the value produced at **6**
  distinct failure points inside `create_with_flags`.
- The kernel already has everything a waiting form needs:
  `scheduler::wait_until(queue, deadline, ready)` with a deadline
  (`kernel/src/scheduler.rs:190-197`), the register/re-check/park handshake that
  closes the lost-wake window, and `sched::waitqs`'s hashed bucket array for
  "a set with no object to hang a queue on" (`FUTEX_BUCKETS`, 64). The keyboard
  read is the in-tree template: `wait_until(&KEYBOARD, deadline, has_data)`
  (`kernel/src/arch/syscall.rs:809-813`).
- `io_uring_enter` already spells a timeout across the boundary: *"0 =
  non-blocking, `u64::MAX` = block forever, else timeout in nanos"*
  (`toyos-abi/src/syscall.rs:1055`). `SYS_CONNECT` passes `0` in `a3` and `a4`
  today, so the register is free.

### The class is already in the tree twice

`services::connect` has 9 call sites outside the test binaries. Two of them are
**hand-rolled boot-retry loops with their own constants**:

- `NetdConn::connect_blocking` — `BOOT_RETRIES = 100`, `BOOT_RETRY_INTERVAL_NS =
  10_000_000` (`toyos/src/net.rs:264-265, 271-279`), 4 call sites.
- `AudioStream::connect_soundd` — the same two constants, same values
  (`toyos/src/audio.rs:188-189, 265-273`).

That is option B's objection already realised: the policy was put in one client
and the next client repeated it, constants and all. It also has its own open
defect — `hardware/network-clients-pay-a-boot-retry`, closed with the loops: on
metal-sim sshd spends 100 `SYS_NANOSLEEP` calls and exits at t=1.69 s on a boot
that completed at 0.38 s, because netd will never come and the loop cannot know.

A third instance is shipping and silent: the compositor spawns `/bin/filepicker`
immediately after `compositor: ready` (`session.rs:220`), and
`filepicker_api::pick_file` connects with `.ok()?`
(`userland/filepicker-api/src/lib.rs:16`) — so an editor that asks for the file
picker before the picker has listened reads it as *the user cancelled*.

### Blast radius, counted

Six boots per suite put `/bin/terminal` in `init` beside the compositor:
`desktop_window_child`, `desktop_typing_damage`, `desktop_locale_detect`
(`tests/desktopcase`), `desktop_audio_client`, `blocked_dump`,
`screen_blocked_dump` (`tests/desktopaudiocase`). Five of the six then call
`shell_answers`/`shell_echoes` and cannot pass without a live terminal; all five
use `compositor: ready` as their `ready_marker`, which is why nothing notices.

The harness already detects the race and names it: `shell_echoes`
(`tests/toyos.rs:4673-4719`) ends its wait on `exit: terminal ` as well as on
`terminal: ready`, so a red arrives in about a second instead of holding a lane
for 305 s. **That is a diagnosis, not a fix** — the run is still red, and no
option below is a reason to remove it.

**The shipping `system.toml` does not contain this pair.** Its init is
`[compositor, soundd, netd]`, and neither daemon connects to the compositor. So
today the terminal race is reachable only from the test configs — and the reason
that is not a reason to fix it in the tests is the next paragraph.

### The honest statement of the defect

`init` is an ordered list that orders nothing. `init = ["/bin/compositor",
"/bin/terminal"]` reads as a sequence and is a spawn order; the config can
express a dependency it cannot honour. Every option below is a different answer
to *who is allowed to know that dependency* — the client, the SDK, the kernel,
the config, or the parent.

## 2. Options

Line counts marked **(measured)** come from `wc -l` or a grep run against this
tree. Counts marked **(estimated)** are estimates and say so.

---

### A. `window::Window::create` retries

Replace the `services::connect` at `userland/window/src/lib.rs:378` with a
bounded retry loop.

- **Changes**: 1 file, `userland/window/src/lib.rs` (618 lines, measured); ~12
  lines added, 2 new constants (estimated). Not a sysroot source, so no ABI-split
  landing.
- **Serves**: nothing much. It is the smallest diff.
- **Strains**: *"Prefer compile-time safety: unrepresentable > checked at runtime
  > covered by tests"* — this is the bottom rung, a poll. And *"the ability to
  iterate fast matters more than feature count"* is not served by a third copy
  of the same loop.
- **Unrepresentable vs checked**: checked, by polling. It makes nothing
  impossible to express.
- **If the compositor never comes**: every window client — `paint`, `files`,
  `editor`, `filepicker`, `snake` through winit — pays the full bound before
  failing, on every machine with no desktop. That is precisely the netd defect,
  generalised to five more programs.
- **The variant that answers the issue file's objection**: make the bound a
  parameter rather than a constant, so `/bin/console`-shaped callers pass zero.
  That is **17 call sites** to edit (5 userland apps, 12 test binaries, measured)
  **plus one in a fork the tree cannot grep** —
  `winit-toyos/src/window.rs:47` calls `Window::create_with_title`. CLAUDE.md:
  *"'I enumerated the call sites' is only true if the enumeration covered
  `~/.cargo/git/checkouts/`."* Adding `Window::create_waiting` instead is 1
  addition and 1 caller.
- **Gate**: a guest binary that creates a window while the compositor is late.
  **Teeth**: hard — the negative control is "the same binary on a tree without
  the loop", i.e. a mutated build, which the project does not have an actuator
  for in userland.

---

### B. `/bin/terminal` retries

The loop at `userland/terminal/src/main.rs:55`.

- **Changes**: 1 file, 196 lines (measured); ~10 lines added (estimated). No
  sysroot claim.
- **Serves**: nothing. It is the smallest correct-looking diff.
- **Strains**: the same rung problem as A, and the tree already demonstrates the
  failure mode — `net.rs` and `audio.rs` are two copies of this decision with
  identical magic numbers and no shared home.
- **Unrepresentable vs checked**: checked, in one program.
- **If the compositor never comes**: the terminal exits after the bound, as
  today, later.
- **Gate/teeth**: same difficulty as A, and the gate would prove a property of
  one binary.

---

### C. The kernel's init loop sequences on service registration

`kernel/src/main.rs:604-610` waits for a declared service before spawning the
next entry; `system.toml`'s `init` gains a provides/needs notation carried
through `src/build.rs:601` (`config.init.join(";")`) into `INIT_PROGRAMS`.

- **Changes**: `kernel/src/main.rs` (7-line loop → ~25, estimated),
  `src/build.rs` `SystemConfig` (lines 19-29) and the `;`-joined serialisation,
  a kernel-side wait for a name — which is option D's mechanism built for one
  caller. There are **11** `system.toml` files (measured); only the two desktop
  test configs need the new notation, the rest must keep parsing. ~80 lines added
  across 4 files (estimated).
- **Serves**: `init` stops lying about being a sequence.
- **Strains**: *"Minimal. New additions to the kernel must be discussed and
  justified."* Boot policy in the kernel is the wrong side of the line, and the
  kernel would need the waiting primitive anyway — so this is D plus kernel
  policy, not an alternative to D.
- **Unrepresentable vs checked**: the *config* becomes checkable at build time
  (see E's gate), but the runtime mechanism is still a wait.
- **If the compositor never comes**: init must decide — skip the dependent
  program and log, or halt. Declared in the config, bounded there.
- **Gate**: a test config whose `init` deliberately lists the client **first**,
  asserting the boot log still shows the provider's `listen` before the client's
  `connect`. **Teeth**: hard. The inverted order is not deterministic on its own
  — the two spawns are 6 ms apart against ~40 ms of startup — so a green run
  proves nothing unless the provider is also made slow, which means a kernel
  feature actuator or a sleep in the provider. This is the one option whose gate
  needs machinery that does not exist. Note also that a host `--lib` config check
  is *not* available here: C's purpose is to make the dependent config work, so
  refusing it at build time would contradict the option.

---

### D. `connect` gains a waiting form and the caller states the bound

**This is the one that needs the owner's approval.**

`SYS_CONNECT` takes a timeout in `a3`, currently ignored and passed as `0`.
`0` keeps today's behaviour exactly; a non-zero value parks the caller until the
name is registered or the deadline passes. A value above `MAX_CONNECT_WAIT_NS`
is `InvalidArgument` — **so "wait forever" is not expressible**, which is the
one part of `io_uring_enter`'s convention (`u64::MAX = block forever`) that
should not be copied.

- **Changes**:
  - `toyos-abi/src/syscall.rs` — `connect` gains the argument; the false doc
    comment at :752-753 is corrected. ~8 lines (estimated). **Sysroot source.**
  - `toyos/src/services.rs` (25 lines, measured) — `connect_within`. ~8 lines
    (estimated). **Sysroot source.**
  - `kernel/src/listener.rs` (231 lines, measured) — a hashed bucket array keyed
    by name, and a wake from `listen`. ~25 lines (estimated).
  - `kernel/src/arch/syscall.rs` — `sys_connect` loops over
    `wait_until(bucket, deadline, || owner(name).is_some())`; `sys_listen`
    (:1246-1252) issues the wake after the registry lock drops, the way
    `wake_poll_waiters` (:1334-1344) already does. ~15 lines (estimated).
  - **Deletions, measured**: `NetdConn::connect_blocking`'s loop and its two
    constants (`toyos/src/net.rs:264-265, 271-279`) and `AudioStream::connect_soundd` and
    its two constants (`toyos/src/audio.rs:188-189, 265-273`) — about 19 lines
    and 4 magic numbers, replaced by one call each. And ~100 `SYS_NANOSLEEP`
    calls per affected client per boot.
  - One caller decision remains: `Window::create` must choose to wait, which is
    A with D's primitive underneath — as `Window::create_waiting` (1 addition, 1
    caller) or as a parameter (17 call sites plus the winit fork).
- **Serves**: *"The kernel ABI and SDK are Rust-native and capability-shaped"* —
  the caller states whether absence is an error or something to wait for, and
  `/bin/console` legitimately says "error". *"Zero technical debt. … Dead code is
  deleted. Every abstraction earns its place."* — it deletes the two retry loops
  rather than adding a third, and *"deleting code wins on that basis alone"*.
- **Strains**: *"Never add or change a syscall without discussion."* And the
  landing: `toyos-abi/src` and `toyos/src` are two of the three
  `SYSROOT_SOURCES` (`src/toolchain.rs:79`), so this **must land as its own pull
  request before the work that uses it** — `--pr` and CI's `abi-split` refuse a
  branch that mixes them (`src/pr.rs:318`).
- **Unrepresentable vs checked**: the connect itself is still checked. What
  becomes unrepresentable is the *hand-rolled retry loop with its own cadence*,
  and the *unbounded* wait. Both are real: two of the former exist today with
  identical constants, and `io_uring_enter` shows the latter is otherwise the
  natural spelling.
- **If the compositor never comes**: the caller's bound expires and it gets
  `NotFound`, one block and one wake rather than 100 sleeps. The terminal then
  exits as it does today, with a shell that lived a second longer. **D does not
  make a terminal survive a compositor that genuinely fails** — only E and F do
  that — it makes it survive one that is merely late.
- **Gate**: `tests/toyos-rust-tests/src/bin/connect_before_listen.rs`, run on
  the shared `tests/testcases` boot (no new boot). A parent spawns a child that
  must connect to a name the parent has not registered; the parent sleeps 200 ms
  — longer than the widest startup-to-`listen` observed anywhere (172 ms, T14) —
  then listens.
  **Teeth, without a mutated tree**: two arms that must answer differently, the
  `hda_client_stall` shape. Arm 1, the child calls the **non-waiting** form and
  must be refused — which is what proves the name really was unregistered at
  that instant. Arm 2, the waiting form on the same name must succeed. A waiting
  form that silently degraded to a plain lookup fails arm 2; a test that passed
  because the server happened to be up already fails arm 1.

---

### E. The server starts its clients

Take `/bin/terminal` out of the two test `init` lists and have the compositor
start it once it is ready, the way it already starts `/bin/filepicker`
(`session.rs:220`) and already spawns a terminal on Ctrl+N
(`KeyAction::SpawnTerminal`, `session.rs:382-383`). The terminal already applies
this discipline to its own child: `Host::listen` at `main.rs:44`, then
`Command::new("/bin/shell")` at `:47`.

- **Changes, narrow form**: 2 lines removed from `tests/desktopcase/system.toml`
  and `tests/desktopaudiocase/system.toml`, plus a way for those configs to say
  "the compositor should autostart this" — the compositor must not grow a
  terminal at boot in the shipping image, which does not have one today. ~15
  lines (estimated) across 3 files.
- **Changes, general form**: a userland `/bin/init` that reads the manifest and
  sequences, so the kernel spawns exactly one program. This is C without kernel
  policy — and it still needs a way to observe that a service is up, i.e. D or a
  poll.
- **Serves**: *"Kernel — Minimal. New additions to the kernel must be discussed
  and justified"*, and the strongest reading of *unrepresentable*: a client that
  is started **by** its server cannot start before it. No check, no bound, no
  timeout.
- **Strains**: *"Zero technical debt"* if taken in the narrow form only — the six
  test boots go green and the class stays open, with the shipping filepicker
  race still in it. And it does not cover a client the user launches, which is
  most of them.
- **If the compositor never comes**: the terminal is never spawned. There is
  nothing to bound. This is the only family with that answer.
- **Gate**: a host `--lib` test in `src/build.rs`, same shape as
  `no_shipped_boot_config_starts_sshd` (`src/build.rs:1095-1105`), refusing any
  config whose `init` names a program that needs a service another `init` entry
  provides. It needs a `provides`/`needs` declaration in `[programs]` to have
  anything to check — the build system cannot infer it. **Teeth**: the negative
  control is a bad config literal in the test body — no mutated tree, no guest,
  milliseconds. Plus a boot-log order assertion in the desktop tests:
  `compositor: ready` must precede `spawn: /bin/terminal`.

---

### F. Hand the client a connection, not a name

The parent connects and passes the socket down through the spawn `fd_map`
(`toyos-abi/src/syscall.rs:111-112`); `Window::create` uses the inherited fd when
there is one. `Descriptor::Socket` is duplicable today
(`kernel/src/fd.rs:123-125`), so **this needs no ABI change at all**.

- **Changes**: `userland/window/src/lib.rs` gains an inherited-fd path (~20
  lines, estimated); whichever parent does the spawning gains the connect and the
  `fd_map` (~10 lines, estimated). Needs E's causal order to have a parent that
  holds a connection.
- **Serves**: the destination named in `specs/assessments/capability-handles-spec.md` §8 —
  possession is the authority, and there is no name to look up and no instant at
  which the lookup can fail. It is also the answer to
  `isolation/capability-by-id-or-name`'s whole class, which is closed.
- **Strains**: nothing in the principles. It strains scope: it covers exactly
  the clients whose parent already holds a connection, and a program the user
  starts from a shell is not one of them.
- **Unrepresentable vs checked**: **unrepresentable**, for the pairs it covers.
  The client never calls `connect`.
- **If the compositor never comes**: the parent's connect fails and the client is
  not spawned.
- **Gate**: assert `terminal: ready` on a boot where the compositor is late,
  plus that the terminal's exit histogram carries no `87=` — `SYS_CONNECT` is 87
  and the kernel already prints the per-process histogram by syscall number
  (`syscalls: pid=3 total=7 syscall_wall=0ms 0=1 6=1 63=1`, from the boot log
  above). **Teeth**: the histogram arm fails the moment the by-name fallback is
  taken, so the test cannot pass on a tree where inheritance quietly stopped
  working. It does constrain the run: a clipboard copy also connects
  (`window::clipboard_set`, `lib.rs:328`), so the arm belongs on a boot that does
  not copy.

---

## 3. On the ABI question: can the kernel tell "not yet" from "never"?

No, and it should not pretend to. A service name is a rendezvous point;
`listener::LISTENERS` (`kernel/src/listener.rs:105-110`) holds names that are
registered and knows nothing about names that will be. Nothing in the system
declares an intention to serve a name, so the kernel has no fact to report.

That means `SyscallError::NotFound` from `connect` is **not** a signature
promising a check it never performs — it truthfully answers *"nothing is serving
that name right now"*, which is the only question the kernel can answer. Two
things in the same path **are** lying, and both are cheap to fix regardless of
which option is chosen:

1. `toyos-abi/src/syscall.rs:752-753` — *"Blocks until the server accepts."*
   `sys_connect` queues the server's end and returns the client's fd.
2. `userland/window/src/lib.rs:80-82` — the 6 `CreateError::NoCompositor` sites
   in `create_with_flags` are one failed lookup, four transport failures and a
   failed `SharedMemory::map`, and the terminal renders all six as
   `terminal: no compositor is running`.

Making "never" knowable would mean a *reservation* — a declaration that a name
will be served, so the kernel could distinguish "declared and pending" from
"nobody is coming". That is real machinery for one boot-time question, and it is
not needed if the knowledge stays where it already lives: `system.toml`'s `init`
list, or the parent that did the spawning. That is the argument for D and E over
anything that tries to teach the kernel the difference.

## 4. Recommendation

**D, with the bound stated at the call site, and E's narrow form beside it.**

D is not a fourth alternative to A, B, C and E — it is the primitive the others
are built on. C needs a kernel-side wait for a name; a userland `/bin/init`
needs the same thing from userland; A and B are that wait open-coded with
`nanosleep`. The tree has already run the experiment: two copies of the loop
exist with the same two constants, and the one with an owner has its own open
defect for burning a second on a machine where the service will never come.
Adding a third copy is the option the evidence argues hardest against.

D's claim to *unrepresentable* is narrower than E's, and it should be stated
narrowly: it does not make the race impossible, it makes the ad-hoc retry loop
and the unbounded wait impossible, and it puts the one remaining decision —
*is absence an error here?* — at the only site that knows the answer.
`/bin/console` says yes and gets today's behaviour with a `0`; `/bin/terminal`
says no and names its bound.

E's narrow form is worth landing beside it because it makes the six affected
boots structurally incapable of the race for about fifteen lines, and because its
build-time gate is the cheapest gate with real teeth on this list. As the *only*
fix it would be a bandaid: it would turn CI green and leave the class that
produced the silent filepicker cancel untouched.

**F is the destination, not the move now.** It is the only genuinely
unrepresentable answer, it needs no ABI change, and it is where
`specs/assessments/capability-handles-spec.md` §8 is already heading — but it covers only
clients whose parent holds a connection, so it cannot be the general answer and
it needs E's restructuring first.

**What would separate D from E-alone, if this is close:** whether `system.toml`'s
`init` is meant to stay "daemons that depend on nothing", or to become the
system's boot description. If the former, E is sufficient and D is machinery for
a case the config will never express. If the latter — and a shipping desktop that
opens a terminal at boot is exactly that case — then `init` needs a dependency
notation, and a dependency notation needs a primitive that can observe a service
coming up, which is D.

**Sequencing, if D is approved.** `toyos-abi/src` and `toyos/src` are two of the
three `SYSROOT_SOURCES`, so D lands as an ABI-only pull request first and the
callers follow on a second — `--pr` and CI's `abi-split` refuse the mixed branch,
and the sysroot claim blocks every other worktree for the duration.
