//! Deterministic scheduler simulator CLI (spec §10).

use std::io::Read;
use std::process::ExitCode;

use toyos_sched_sim::choice::ChoiceStream;
use toyos_sched_sim::explore::run;
use toyos_sched_sim::scenarios;
use toyos_sched_sim::shrink;
use toyos_sched_sim::sweep;

const USAGE: &str = "\
usage: toyos-sched-sim <command> [args]
  run <scenario> [seed]        one seeded exploration
  pct <scenario> [seed]        one PCT-driven exploration
  fuzz <scenario>              decisions driven by raw fuzz bytes on stdin
  sweep [seeds]                seed sweep over every scenario (default 10000)
  fuzz-sweep [steps]           fuzz-byte sweep per scenario (default 10000000)
  gate [seeds]                 the Stage 4 exit criterion, including the
                               negative old_steal_port and
                               old_commit_before_pass gates
  shrink <scenario> <seed> [pct]
                               minimize a failing seed into a corpus trace
  replay <file>                replay a committed corpus trace
  list                         scenario names
  from-qemu <trace.bin>        convert a kernel TraceEvent capture";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    match cmd {
        "list" => {
            for scenario in scenarios::all() {
                println!("{}", scenario.name);
            }
            println!("old_steal_port          (negative gate: must fail)");
            println!("old_commit_before_pass  (negative gate: must fail)");
            println!("old_commit_fused        (control: passes, and that is the point)");
            ExitCode::SUCCESS
        }
        "run" | "pct" => {
            let Some(scenario) = args.get(1).and_then(|n| scenarios::by_name(n)) else {
                eprintln!("unknown scenario; try `list`");
                return ExitCode::FAILURE;
            };
            let seed: u64 = args.get(2).map_or(0, |s| s.parse().unwrap_or(0));
            let mut choices = if cmd == "pct" {
                ChoiceStream::pct(seed, scenario.cpus, 3)
            } else {
                ChoiceStream::from_seed(seed)
            };
            let outcome = run(scenario, &mut choices);
            println!("{}", outcome.report());
            ok(outcome.passed())
        }
        "fuzz" => {
            let Some(scenario) = args.get(1).and_then(|n| scenarios::by_name(n)) else {
                eprintln!("unknown scenario; try `list`");
                return ExitCode::FAILURE;
            };
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .expect("reading fuzz bytes");
            let mut choices = ChoiceStream::from_bytes(bytes);
            let outcome = run(scenario, &mut choices);
            println!("{}", outcome.report());
            ok(outcome.passed())
        }
        "sweep" | "fuzz-sweep" | "gate" => {
            let budget: u64 = args.get(1).map_or(
                if cmd == "fuzz-sweep" {
                    10_000_000
                } else {
                    10_000
                },
                |s| s.parse().unwrap_or(10_000),
            );
            let mut clean = true;
            for scenario in scenarios::all() {
                let result = if cmd == "fuzz-sweep" {
                    sweep::fuzz_sweep(&scenario, budget, 3)
                } else {
                    sweep::seed_sweep(&scenario, budget, 3)
                };
                println!("{}", result.report());
                clean &= result.passed();
            }
            if cmd == "gate" {
                // The negative gates need far fewer schedules than the
                // positive sweep: they only have to be caught, not proven
                // absent.
                for negative in [scenarios::old_steal_port(), scenarios::old_commit_before_pass()] {
                    let name = negative.name;
                    let result = sweep::seed_sweep(&negative, budget.min(500), 1);
                    let found = !result.passed();
                    println!(
                        "{name}: {} runs, {}",
                        result.runs,
                        if found {
                            "caught (as required)"
                        } else {
                            "NOT CAUGHT — the harness proves nothing"
                        },
                    );
                    if let Some(first) = result.failures.first() {
                        println!("  {}", first.violations.join("\n  "));
                    }
                    clean &= found;
                }
                // And the control: the same shape with the block's two halves
                // fused into one step must come back *clean*, because that is
                // the blind spot this harness used to have.
                let control = sweep::seed_sweep(&scenarios::old_commit_fused(), budget.min(500), 1);
                println!(
                    "old_commit_fused: {} runs, {}",
                    control.runs,
                    if control.passed() {
                        "clean (the control: no step boundary, no bug in sight)"
                    } else {
                        "FAILED — the control no longer controls for anything"
                    },
                );
                clean &= control.passed();
            }
            ok(clean)
        }
        "shrink" => {
            let Some(scenario) = args.get(1).and_then(|n| scenarios::by_name(n)) else {
                eprintln!("unknown scenario; try `list`");
                return ExitCode::FAILURE;
            };
            let seed: u64 = args.get(2).map_or(0, |s| s.parse().unwrap_or(0));
            let mut choices = if args.get(3).is_some_and(|d| d == "pct") {
                ChoiceStream::pct(seed, scenario.cpus, 3)
            } else {
                ChoiceStream::from_seed(seed)
            };
            let outcome = run(scenario.clone(), &mut choices);
            if outcome.passed() {
                eprintln!("seed {seed} does not fail; nothing to shrink");
                return ExitCode::FAILURE;
            }
            let minimized = shrink::shrink(&scenario, outcome.decisions);
            eprintln!(
                "shrunk to {} decisions; violations:\n  {}",
                minimized.len(),
                outcome.violations.join("\n  "),
            );
            print!("{}", shrink::encode(scenario.name, true, &minimized));
            ExitCode::SUCCESS
        }
        "replay" => {
            let Some(path) = args.get(1) else {
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            };
            let text = std::fs::read_to_string(path).expect("reading the trace");
            let entry = shrink::decode(&text);
            let scenario =
                scenarios::by_name(&entry.scenario).expect("the trace names a known scenario");
            let outcome = shrink::replay(&entry, scenario);
            println!("{}", outcome.report());
            ok(outcome.passed() != entry.expect_failure)
        }
        "from-qemu" => unimplemented!(
            "from-qemu: the kernel's TraceEvent ring lands at migration Stage 6 \
             (specs/scheduler-core-spec.md §11); there is nothing to convert yet"
        ),
        other => {
            eprintln!("unknown command {other:?}\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn ok(passed: bool) -> ExitCode {
    if passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
