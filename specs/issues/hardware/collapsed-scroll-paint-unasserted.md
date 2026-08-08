---
status: open
kind: defect
opened: 2026-08-03
---

# The collapsed-scroll paint is reached only by the boot seed, and nothing asserts the panel after it

`Console::write_bytes` flushes once per call, so a batch carrying more rows than
the screen has scrolls the cell grid many times and reaches the panel once. That
path is the reason `Console` composes in RAM at all, and `screen_console_scroll`
was written believing its last round — a single 1200-line write — exercised it.
It does not. The console reads its shell's pipe with `let mut buf = [0u8; 4096]`
and one `read` per poll pass, so the batch it sees is capped there whatever the
writer does: instrumented on 2026-08-03 at 1781 bytes a batch when the writer
flushed every 7 lines and **2870** when it wrote all 1200 at once — 112 reads for
one write. At ~257 bytes a line that is 11 lines, against the 66 rows a collapse
needs.

The only caller that does reach it is `seed_kernel_log`, which hands the console
up to 64 KiB in one `write_bytes` at startup — a thousand rows scrolled into a
single paint. No test asserts the panel after the seed: `screen_console_shell`
and `screen_console_scroll` both wait for the prompt and then assert on what
comes *after* it. So the path exists, runs on every console boot, and is checked
by nothing.

Reaching it from a test needs a writer inside the console's own process, or a
console read larger than a batch. Not a defect in the console; a hole in what
the screen family covers, and the workload comment that claimed otherwise is
corrected as of this entry.
