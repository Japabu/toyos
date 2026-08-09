//! What every program in an image is allowed to hold, written by the build
//! system and read by `/bin/init`.
//!
//! **One definition of the format, used by both halves.** `src/build.rs`
//! resolves `system.toml` into a [`Manifest`] and [`render`]s it into the
//! initrd at [`PATH`]; init [`parse`]s it back. A round-trip test here is what
//! makes that a fact rather than two hand-matched implementations — the shape
//! this crate exists to prevent is a renderer and a parser that disagree about
//! one record and a machine that boots with an authority nobody declared.
//!
//! Line-oriented and deliberately not TOML: the build system already has the
//! parsed config, so a TOML parser in the guest would be a dependency for a
//! job already done.
//!
//! ```text
//! program <name> <path>     starts a program's records
//! arg <text>                argv after argv[0]
//! serve <name>              init makes one machine-wide port and endows the acceptor
//! provide <name>            this program makes its own port, once per instance
//! receive <name>            a connector in this program's namespace
//! device <class>            a claim init mints and endows
//! realtime                  init endows an RT-only SysCap dup
//! init-serve <name>         a name init serves itself
//! start <name>              init starts this program at boot
//! ```

/// Where the initrd carries it, without a leading slash — the initrd's own
/// spelling. [`GUEST_PATH`] is what a process opens.
pub const PATH: &str = "etc/system.manifest";

/// The path `/bin/init` opens.
pub const GUEST_PATH: &str = "/etc/system.manifest";

/// A program key may be this long. Policy on the primitive: the launcher
/// carries one in a message, and a longer one is refused by name rather than
/// truncated into some other program's.
pub const MAX_PROGRAM_NAME: usize = 32;

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Program {
    pub name: String,
    pub path: String,
    pub args: Vec<String>,
    /// Machine-wide ports init creates and endows the **acceptor** of.
    pub serves: Vec<String>,
    /// Ports this program makes for itself, once per instance. init creates
    /// nothing and holds nothing for these.
    pub provides: Vec<String>,
    /// Names in this program's namespace, each a connector.
    pub receives: Vec<String>,
    pub devices: Vec<String>,
    pub realtime: bool,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Sorted by name, which is what makes [`render`] byte-for-byte
    /// deterministic.
    pub programs: Vec<Program>,
    /// Names init serves itself. init is in every image and is no `[programs]`
    /// key, so these have no declaration to come from.
    pub init_serves: Vec<String>,
    /// Program names, in the order `[boot] start` gave them — which orders
    /// nothing, because every port exists before any server runs.
    pub start: Vec<String>,
}

impl Manifest {
    pub fn program(&self, name: &str) -> Option<&Program> {
        self.programs.iter().find(|p| p.name == name)
    }

    /// Every `serves` name in the whole manifest, not only the ones [`start`]
    /// names: the filepicker is launched by the compositor, and an editor
    /// holding its connector must be able to ask for a file before the picker
    /// has run an instruction.
    ///
    /// [`start`]: Self::start
    pub fn served_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .programs
            .iter()
            .flat_map(|p| p.serves.iter().map(String::as_str))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// Why a manifest could not be rendered.
///
/// The build system is the only caller and a failure stops the image: a name
/// with a space in it would parse back as a different record, so it is refused
/// where it is written rather than discovered where it is read.
#[derive(Debug, PartialEq, Eq)]
pub enum RenderError {
    NameTooLong(String),
    /// A field whose bytes would not survive the round trip.
    Unrepresentable { program: String, field: &'static str, value: String },
}

pub fn render(manifest: &Manifest) -> Result<Vec<u8>, RenderError> {
    let mut out = String::new();
    for program in &manifest.programs {
        if program.name.len() > MAX_PROGRAM_NAME {
            return Err(RenderError::NameTooLong(program.name.clone()));
        }
        check(&program.name, "name", &program.name)?;
        check(&program.name, "path", &program.path)?;
        out.push_str(&format!("program {} {}\n", program.name, program.path));
        for arg in &program.args {
            reject_newline(&program.name, "args", arg)?;
            out.push_str(&format!("arg {arg}\n"));
        }
        for (field, values) in [
            ("serves", &program.serves),
            ("provides", &program.provides),
            ("receives", &program.receives),
            ("devices", &program.devices),
        ] {
            let word = match field {
                "serves" => "serve",
                "provides" => "provide",
                "receives" => "receive",
                _ => "device",
            };
            for value in values {
                check(&program.name, field, value)?;
                out.push_str(&format!("{word} {value}\n"));
            }
        }
        if program.realtime {
            out.push_str("realtime\n");
        }
    }
    for name in &manifest.init_serves {
        check("init", "init_serves", name)?;
        out.push_str(&format!("init-serve {name}\n"));
    }
    for name in &manifest.start {
        check("init", "start", name)?;
        out.push_str(&format!("start {name}\n"));
    }
    Ok(out.into_bytes())
}

/// A field that becomes a whole record: no whitespace at all, because the
/// parser splits the record word off at the first space.
fn check(program: &str, field: &'static str, value: &str) -> Result<(), RenderError> {
    if value.is_empty() || value.contains(char::is_whitespace) {
        return Err(RenderError::Unrepresentable {
            program: program.to_string(),
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// An argument is the rest of its line, so it may hold spaces and may not hold
/// a newline.
fn reject_newline(program: &str, field: &'static str, value: &str) -> Result<(), RenderError> {
    if value.contains('\n') {
        return Err(RenderError::Unrepresentable {
            program: program.to_string(),
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Read back what [`render`] wrote.
///
/// Panics on a line neither half can produce. This file is the build system's
/// own output travelling in the image beside the binary that reads it, so a
/// malformed record is a bug in the pair rather than untrusted input.
pub fn parse(text: &str) -> Manifest {
    let mut manifest = Manifest::default();
    for line in text.lines() {
        let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
        match word {
            "program" => {
                let (name, path) = rest
                    .split_once(' ')
                    .unwrap_or_else(|| panic!("manifest: `program` without a path: {line}"));
                assert!(
                    name.len() <= MAX_PROGRAM_NAME,
                    "manifest: program name longer than {MAX_PROGRAM_NAME}: {name}"
                );
                manifest.programs.push(Program {
                    name: name.to_string(),
                    path: path.to_string(),
                    ..Program::default()
                });
            }
            "init-serve" => manifest.init_serves.push(rest.to_string()),
            "start" => manifest.start.push(rest.to_string()),
            "" => {}
            _ => {
                let program = manifest
                    .programs
                    .last_mut()
                    .unwrap_or_else(|| panic!("manifest: `{word}` before any program"));
                match word {
                    "arg" => program.args.push(rest.to_string()),
                    "serve" => program.serves.push(rest.to_string()),
                    "provide" => program.provides.push(rest.to_string()),
                    "receive" => program.receives.push(rest.to_string()),
                    "device" => program.devices.push(rest.to_string()),
                    "realtime" => program.realtime = true,
                    other => panic!("manifest: unknown record `{other}`"),
                }
            }
        }
    }
    manifest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            programs: vec![
                Program {
                    name: "compositor".into(),
                    path: "/bin/compositor".into(),
                    serves: vec!["compositor".into()],
                    receives: vec!["soundd".into(), "launcher".into()],
                    devices: vec!["framebuffer".into(), "keyboard".into()],
                    ..Program::default()
                },
                Program {
                    name: "soundd".into(),
                    path: "/bin/soundd".into(),
                    serves: vec!["soundd".into()],
                    devices: vec!["hda-audio".into(), "virtio-sound".into()],
                    realtime: true,
                    ..Program::default()
                },
                Program {
                    name: "terminal".into(),
                    path: "/bin/terminal".into(),
                    args: vec!["--login shell".into()],
                    provides: vec!["surface".into()],
                    receives: vec!["compositor".into()],
                    ..Program::default()
                },
            ],
            init_serves: vec!["launcher".into()],
            start: vec!["compositor".into(), "soundd".into()],
        }
    }

    /// The one property both halves depend on, and the reason they live here.
    #[test]
    fn what_the_build_writes_is_what_init_reads() {
        let manifest = sample();
        let rendered = render(&manifest).expect("render");
        assert_eq!(parse(std::str::from_utf8(&rendered).unwrap()), manifest);
    }

    #[test]
    fn the_same_manifest_renders_to_the_same_bytes() {
        assert_eq!(render(&sample()), render(&sample()));
    }

    #[test]
    fn records_attach_to_the_program_above_them() {
        let m = parse(
            "program soundd /bin/soundd\nserve soundd\nrealtime\n\
             program toybox /bin/toybox\narg pwd\nreceive compositor\n\
             init-serve launcher\nstart soundd\n",
        );
        assert!(m.program("soundd").unwrap().realtime);
        assert!(!m.program("toybox").unwrap().realtime);
        assert_eq!(m.program("toybox").unwrap().args, ["pwd"]);
        assert_eq!(m.served_names(), ["soundd"]);
    }

    /// A name with a space in it parses back as a different record, so it is
    /// refused where it is written.
    #[test]
    fn a_name_that_would_not_survive_the_round_trip_is_refused() {
        let mut bad = sample();
        bad.programs[0].serves = vec!["two words".into()];
        assert!(matches!(render(&bad), Err(RenderError::Unrepresentable { .. })));

        let mut long = sample();
        long.programs[0].name = "x".repeat(MAX_PROGRAM_NAME + 1);
        assert!(matches!(render(&long), Err(RenderError::NameTooLong(_))));

        let mut newline = sample();
        newline.programs[2].args = vec!["a\nb".into()];
        assert!(matches!(render(&newline), Err(RenderError::Unrepresentable { .. })));
    }
}
