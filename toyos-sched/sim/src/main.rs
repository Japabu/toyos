//! Deterministic scheduler simulator CLI (spec §10). The virtual machine,
//! explorer, shrinker and scenario library land at migration Stage 4; until
//! then every subcommand that needs them dies loudly rather than pretending.

use std::process::ExitCode;

const USAGE: &str = "\
usage: toyos-sched-sim <command> [args]
  run --seed <u64>       one seeded exploration
  fuzz                   decisions driven by raw fuzz bytes on stdin
  replay <trace>         replay a recorded decision trace
  shrink <trace>         delta-debug a failing trace to a minimal repro
  from-qemu <trace.bin>  convert a kernel TraceEvent capture into a script";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    match cmd.as_str() {
        "run" | "fuzz" | "replay" | "shrink" | "from-qemu" => unimplemented!(
            "toyos-sched-sim {cmd}: the simulator VM lands at migration Stage 4 \
             (specs/scheduler-core-spec.md §11)"
        ),
        other => {
            eprintln!("unknown command {other:?}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}
