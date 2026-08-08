---
status: open
kind: finding
opened: 2026-08-05
---

# `Sink::append`'s error return is correct and no longer reachable from a boot

Task #140 replaced the single appended `kernel.log` with one file per boot. The
sink therefore always *creates*, and within a boot its own pages stay resident —
every append sets the CLOCK reference bit on the page it is appending to, so it
is the last page eviction would take. The partial write into a page that has to
come off the stick, which is what `file_cache::write_page` re-reads for, is
consequently something the sink no longer does.

What that costs is one link of a chain, not the hazard: `write_page`'s
merge-into-a-failed-read is unchanged and is reached by anything appending to a
file that already has bytes on the volume, which is what
`log_backing_read_error` now stages — the host writes the file, a process
appends inside it, and the refusal has to reach that process. The claim that
went untested with the trigger is the propagation through `Sink::append` →
`Sink::flush` → `poll` and the sink disabling itself. That code is still there
and still correct by inspection; nothing exercises it.

Reaching it again needs the sink to append to a file with bytes already on the
device, which no shipped path now produces. Worth revisiting if `log_file` ever
grows a resume-an-existing-file case; not worth contriving one for.
