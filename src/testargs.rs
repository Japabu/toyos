//! The suite's command line, checked against the flags it actually has.
//!
//! `tests/toyos.rs` reads its flags by name and takes the first remaining
//! positional argument as the run's filter. A flag it does not have therefore
//! costs nothing and its *value* becomes that filter, so a command line naming
//! a deleted flag runs one test and reports the run as a pass. `--land` prints
//! that the gate was not the default and nothing else refuses it.
//!
//! So the flag table is here, one entry per flag the harness reads, and the
//! filter falls out of the same pass rather than out of a second guess about
//! which words were already spoken for. A flag added to the harness and not to
//! this table is refused the first time anyone types it — the drift that is
//! loud rather than the one that narrows a gate.

/// Whether a flag is followed by a separate word, which is then not the filter.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Value {
    None,
    Required,
}

pub struct Flag {
    pub name: &'static str,
    pub value: Value,
}

const fn flag(name: &'static str, value: Value) -> Flag {
    Flag { name, value }
}

/// Every flag `tests/toyos.rs` reads, and nothing else.
pub const FLAGS: &[Flag] = &[
    flag("--debug", Value::None),
    flag("--list", Value::None),
    flag("--nocapture", Value::None),
    flag("--show-output", Value::None),
    flag("--audio-gate", Value::Required),
    flag("--jobs", Value::Required),
    flag("-j", Value::Required),
    flag("--host-slots", Value::Required),
];

fn accepted() -> String {
    FLAGS
        .iter()
        .map(|f| match f.value {
            Value::None => f.name.to_string(),
            Value::Required => format!("{} <value>", f.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Validate the harness's argv and return the run's filter.
///
/// `Err` is a refusal to print and exit on. It is asked before the sysroot lock
/// and before anything is compiled, so a stale command line costs a message
/// rather than a queue behind it.
pub fn parse(args: &[String]) -> Result<Option<&str>, String> {
    let mut filter: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        if !arg.starts_with('-') {
            if let Some(first) = filter {
                return Err(format!(
                    "{first:?} and {arg:?}: the suite takes one filter, and the second word \
                     would have been dropped in silence.\n\
                     A filter is a substring, so `{first}` and `{arg}` are one run only if one \
                     substring matches both."
                ));
            }
            filter = Some(arg);
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, _)) => (name, true),
            None => (arg, false),
        };
        let Some(f) = FLAGS.iter().find(|f| f.name == name) else {
            return Err(format!(
                "{arg}: the suite has no such flag, and an unknown flag's value becomes the \
                 run's filter — so this would have measured whatever one test it named.\n\
                 Flags it has: {}.",
                accepted()
            ));
        };
        if inline && f.value == Value::None {
            return Err(format!("{arg}: {name} takes no value.\nFlags it has: {}.", accepted()));
        }
        if !inline && f.value == Value::Required {
            i += 1;
        }
    }
    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_owned(args: &[&str]) -> Result<Option<String>, String> {
        let owned: Vec<String> = args.iter().map(ToString::to_string).collect();
        parse(&owned).map(|f| f.map(ToString::to_string))
    }

    /// The incident: `--skip` was deleted with the expected-failure declaration,
    /// and every handover still carried it.
    #[test]
    fn a_deleted_flag_is_refused_rather_than_becoming_the_filter() {
        let refusal = parse_owned(&["--skip", "desktop_window_child"]).unwrap_err();
        assert!(refusal.starts_with("--skip:"), "{refusal}");
        assert!(refusal.contains("--jobs <value>"), "{refusal}");
    }

    #[test]
    fn a_flags_value_is_not_the_filter() {
        assert_eq!(parse_owned(&["--jobs", "4"]).unwrap(), None);
        assert_eq!(parse_owned(&["-j", "4"]).unwrap(), None);
        assert_eq!(parse_owned(&["--audio-gate", "30"]).unwrap(), None);
        assert_eq!(parse_owned(&["--host-slots", "0"]).unwrap(), None);
    }

    #[test]
    fn the_filter_is_the_word_that_is_nobodys_value() {
        assert_eq!(parse_owned(&["process_stats"]).unwrap().as_deref(), Some("process_stats"));
        assert_eq!(
            parse_owned(&["--audio-gate", "30", "audio_tone", "--nocapture"]).unwrap().as_deref(),
            Some("audio_tone")
        );
        assert_eq!(
            parse_owned(&["--jobs=4", "futex", "--show-output"]).unwrap().as_deref(),
            Some("futex")
        );
    }

    #[test]
    fn two_filters_are_refused_because_only_one_would_run() {
        let refusal = parse_owned(&["futex", "dlopen"]).unwrap_err();
        assert!(refusal.contains("\"futex\"") && refusal.contains("\"dlopen\""), "{refusal}");
    }

    #[test]
    fn an_inline_value_on_a_flag_that_has_none_is_refused() {
        let refusal = parse_owned(&["--nocapture=1"]).unwrap_err();
        assert!(refusal.contains("--nocapture"), "{refusal}");
    }

    #[test]
    fn the_documented_command_lines_parse() {
        for argv in [
            vec![],
            vec!["--nocapture"],
            vec!["process_stats"],
            vec!["process_stats", "--nocapture"],
            vec!["--list"],
            vec!["--audio-gate", "30"],
            vec!["--jobs", "4"],
            vec!["--host-slots", "0"],
            vec!["--debug"],
        ] {
            assert!(parse_owned(&argv).is_ok(), "{argv:?}");
        }
    }
}
