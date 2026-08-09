---
status: open
kind: defect
opened: 2026-08-08
---

# Soundd spells a past-due wake `.max(1)`, which is a one-nanosecond timer

`userland/soundd/src/main.rs:955` and `:1327` both compute a poll timeout as
`…saturating_sub(clock_nanos()).max(1)`, with a comment saying the `1` is there
because `0` is the kernel's non-blocking sentinel. But non-blocking is exactly
what a grid point already in the past wants, so `0` is the right answer and `1`
is a park on a deadline that has passed. It is the trigger on boot 5 of
`specs/metal-logs/2026-08-08-cpu0/`, where cpu0 and cpu1 both stopped within
100 ms of `soundd: resumed`.

Not fixed here, and deliberately: it is one line in the mix loop, `apic::OneShot`'s
floor makes it safe, and what it changes is when soundd wakes on a late period —
audio timing, which is the owner's call and which gate A's thorough tier cannot
currently adjudicate (`specs/issues/audio/`). With the floor, a past-due grid point now costs up to
10 µs of extra lateness against a 2.9 ms period.
