---
status: open
kind: defect
opened: 2026-08-10
---

# A binary a machine test *drives* can also sit in the shared registry, where it has no verdict

`check_no_collisions` (`tests/toyos.rs`) refuses a shared-boot binary whose
**name** is also a registered machine or screen test. It cannot see the other
shape: a binary a machine test drives under a *different* name. Such a binary is
still discovered by `discover_rust_tests`, still runs on the shared boot, and
there passes on its exit code with nothing staged for it to act on.

Two instances, found by scanning `tests/toyos.rs` and `tests/common/*.rs` for
`test_rs_<name>` and intersecting with the shared registry:

- **`log_volume_reread`** — driven by `log_backing_read_error`
  (`tests/common/volumes.rs:951`), which stages `/log/staged-reread.txt` on the
  volume before the machine exists. On the shared boot nothing stages it, so the
  program prints `reread: /log/staged-reread.txt did not open: …`, returns, and
  passes. 11 ms in `tests/test-durations`.
- **`test_screen_graffiti`** — driven by `screen_console_clear`. Fixed in the
  same commit as this file was written, because the suite split moved it: it
  calls `SYS_DEBUG` and would otherwise have been forced onto the actuator boot,
  still vacuous. It is in `RUST_SKIP` now.

Three more binaries are driven *and* in the shared registry and are **not**
instances — each asserts something real in both places, and `wall_clock_now`'s
own doc comment says it runs on four machines deliberately: `null_sink_client_exits`,
`nvme_home_roundtrip`, `std_alloc`, `wall_clock_now`.

**The gate that would catch it** is the shape `suite_split` already uses: read
the harness for `test_rs_<name>`, intersect with the shared registry, and require
each hit to declare — either `RUST_SKIP` with the reason its driver exists, or a
statement that its shared-boot run has a verdict of its own. The list of
legitimate double-runs is four names long, so declaring them is cheap; what is
not cheap is the class, which `specs/assessments/test-cost-audit.md` §5.2 already found once
(`test_screen_churn`) and which nothing has been watching since.
