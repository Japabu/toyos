---
status: open
kind: track
opened: 2026-08-18
---

# The log staged three things it never built, and userland output still never reaches logd

The log architecture is on the tree: the per-CPU record ring, `klogd`, the
cursor syscall, the per-holder console object, and `/bin/logd` owning `/log`.
Three staged pieces were never built.

**1. Userland stdout and stderr are console writes, not streams to logd.** Every
program's slots 1 and 2 are a `ConsoleObject` minted at spawn, so a `println!`
reaches the backend directly and `/log` carries kernel records only. What was
designed instead: init creates a pipe pair per `[boot] start` program, sends the
read ends to `logd` over `SYS_HANDLE_SEND` with a frame naming the program, and
endows the write ends as the child's slots; the launcher and sshd do the same
for what they spawn.

None of what that needs exists:

- `serves = ["log"]` on logd's manifest row, and the acceptor with it. **It is
  absent on purpose** — an acceptor with no client is a port that exists for a
  plan, and nothing sends a frame today.
- The frame itself, a label bound, a stream count, and a per-stream bounded
  backlog, so a flooding process fills only its own pipe.
- The std PAL mapping `Gone` on slots 1 and 2 to a successful write of zero
  bytes, so a dead `logd` does not panic every daemon through `println!`.
- init holding logd's `Process` handle and naming its exit on the console —
  the only thing that would tell a machine its userland output has stopped going
  anywhere.

It also unblocks two tests that cannot be written before it: `logd_gone`, and
the console half of a shutdown's last line.

**2. Two negative controls have no instrument to red.** Both were deferred
rather than dropped, and each needs a second half. `log-writes-the-file` puts a
kernel context back to appending records to `/log` through the VFS — the
coupling the whole design removed, rebuilt in miniature — and reds an I/O-depth
measurement and an audio-latency A/B, neither of which is taken today; nothing
else in the tree can stage a kernel that writes a file, so it is the one control
of its class. `log-trusts-durable` removes the clamp on the durability timestamp
a reader publishes, but removing it alone changes nothing: the clamp bites only
against a reader publishing past the newest record, and the one reader a shipped
image has publishes what it synced. It needs a userland knob that publishes a
bad value as well as a kernel that believes one.

**3. A persistent-RAM region for the previous boot's records.** A region
excluded from the memory map, into which the panic path copies the merged record
tail with a header and a checksum; the next boot validates it, answers for it
under a flag on the cursor syscall, and `logd` writes it out. The format is the
record array byte for byte — no second serialisation and no second formatter.

It closes what the bounded wait for `logd` cannot: a panic no scheduler can
answer, a panic while any CPU holds a lock `logd` needs, a `logd` that died
earlier in the boot, a double fault, a triple fault. On a machine with no serial
port those are the boots whose only record is a photograph of a panel.

**Its value turns on a firmware behaviour nothing here can observe, and that is
why it is separate work.** A guest reset preserves guest RAM, so a test
certifies the format and the code path and says nothing about what a real
machine's firmware does to the region on reset. Folding it in would make the
rest depend on a question it cannot answer; the metal arm is owed either way.
