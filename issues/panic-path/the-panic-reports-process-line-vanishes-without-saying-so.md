---
status: open
kind: finding
opened: 2026-08-22
---

# A panic report's `Process:` line vanishes without saying so

`crash_report_panic` (`kernel/src/arch/idt/exceptions.rs`) prints the faulting
process's name and teardown state from the process table, under a `try_lock` it
is right not to turn into a wait:

```rust
if let Some(guard) = process::PROCESS_TABLE.try_lock() {
    if let Some(table) = guard.as_ref() {
        if let Some(proc) = table.get(pid) {
            log!("  Process: {} pid={} state={}", ...);
        }
    }
}
```

Three `if let`s and no `else`. When the `try_lock` loses, the line is simply not
there, and nothing in the report says a line was owed — which is the shape
`process::SymbolLookup` exists to make unwritable for a symbol, one function
over. A reader of a report with no `Process:` line cannot tell "the table was
held" from "this pid is not in the table" from "there is no such line in this
kind of report".

`process::dump_crash_diagnostics` is the same `try_lock` done honestly:
`[crash diagnostics: PROCESS_TABLE locked, skipping]`.

## Why it is a finding and not a defect

Nothing measured it losing. It is one line, and everything it carries but the
*name* is elsewhere in the same report (`Running: pid=… tid=…` is printed
immediately above it, from percpu, with no lock). The name is worth having and
worth an `else` — the fix is two lines — but no gate asserts on the line today
and no run on record is missing it.

Found while removing the symbol lookup's dependency on the same table (PR #239,
`kernel/src/process.rs`'s module header). Not fixed there: that change is about
names, and this is a different line with no measurement behind it.
