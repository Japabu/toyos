---
status: open
kind: defect
opened: 2026-08-03
---

# The bound is one generation, and after a rotation the newest bytes are in the older-looking file

`kernel.log` rotates to `kernel.log.1` at 4 MiB and the previous `.1` is
deleted. A rotation can be the last thing a boot does, which leaves
`kernel.log` empty and the tail in `kernel.log.1` — so anything reading the log
has to read both. `kernel_log_file` asserts the shutdown's last line is in one of
them rather than in `kernel.log`, for that reason.
