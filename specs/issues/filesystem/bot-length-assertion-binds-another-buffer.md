---
status: open
kind: defect
opened: 2026-08-02
---

# `bot`'s length assertion names `MSC_DATA_LEN` and binds a different buffer

Filed, not fixed. `bot` asserts `data_len as usize <= MSC_DATA_LEN` (32 KiB),
and four of its five call sites point at `MSC_SCRATCH`, whose length is 64.
The assertion permits a 32,768-byte transfer into a 64-byte buffer. Today's
largest is 36 (INQUIRY) so there is no live bug; the next command added is
where it becomes one, and the assertion is what the person adding it will read
to decide the buffer is big enough. Same shape as `IpcPayload`: a bound in the
right place with the wrong operand. The fix is to give `bot` the *region*
rather than a physical address it cannot reason about. `usb-storage.md` F6.
