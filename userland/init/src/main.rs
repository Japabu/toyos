//! The one program the kernel starts, and the only holder of the machine's
//! system capability.
//!
//! Everything else is started from here, holding exactly what
//! `/etc/system.manifest` says it holds. **Every port exists before any server
//! runs**: init creates one per `serves` name in the whole manifest, then
//! builds each program's namespace out of the connectors and spawns it with the
//! acceptor moved in. So a client's connection works from its first
//! instruction whether or not the server has reached `accept` or has even been
//! spawned, there is no instant at which a name is not bound yet, and there is
//! nothing anywhere to retry.

/// One line, one `write`.
///
/// **`eprintln!` is not one write.** Stderr is unbuffered by design, so
/// `write_fmt` issues a syscall per format fragment, and on this machine the
/// console and the kernel's log ring are one stream — so a daemon's own line
/// lands inside init's. `netd: ready, at most ` and `init: started test-runner`
/// arrived interleaved and the harness parsed a cap out of the wrong number.
/// `userland/soundd` has the same macro for the same reason.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let mut line = format!($($arg)*);
        line.push('\n');
        let _ = std::io::stderr().write_all(line.as_bytes());
    }};
}

use std::collections::BTreeMap;
use std::os::toyos::process::CommandExt;
use std::process::Command;

use toyos_manifest::{Manifest, Program};
use toyos::endow::Endowments;
use toyos::namespace::{self, Namespace};
use toyos::port::{self, Acceptor, Connector};
use toyos::syscap::SysCap;
use toyos_abi::syscall::{DeviceType, DEV_PREFIX, SERVE_PREFIX, SVC_LABEL, SYSCAP_LABEL};

/// The service init answers on. Its own, so it has no `[programs]` row and the
/// manifest carries it as an `init-serve` record.
const LAUNCHER: &str = "launcher";

fn main() {
    let syscap: SysCap = Endowments::get()
        .take(SYSCAP_LABEL)
        .expect("init: the kernel spawns this program holding the system capability");

    let text = std::fs::read_to_string(toyos_manifest::GUEST_PATH)
        .unwrap_or_else(|e| panic!("init: cannot read {}: {e}", toyos_manifest::GUEST_PATH));
    let system = toyos_manifest::parse(&text);

    // Before anything is spawned, and for every `serves` name in the manifest
    // rather than only the ones `[boot] start` names: the filepicker is
    // launched by the compositor, and an editor holding its connector must be
    // able to ask for a file before the picker has run an instruction.
    let mut acceptors = BTreeMap::new();
    let mut connectors: BTreeMap<&str, Connector> = BTreeMap::new();
    let names = system
        .served_names()
        .into_iter()
        .chain(system.init_serves.iter().map(String::as_str));
    for name in names {
        let (acceptor, connector) =
            port::create().unwrap_or_else(|e| panic!("init: no port for `{name}`: {e:?}"));
        acceptors.insert(name, acceptor);
        connectors.insert(name, connector);
    }

    for name in &system.start {
        let program = system
            .program(name)
            .unwrap_or_else(|| panic!("init: [boot] start names `{name}`, which is not declared"));
        start(program, &system, &syscap, &mut acceptors, &connectors);
    }

    // Nothing else holds a `serves` acceptor that has not been launched yet, so
    // init outliving its children is what keeps those ports open. It parks
    // here.
    let launcher = acceptors
        .remove(LAUNCHER)
        .expect("init: the manifest declares init serves `launcher`");
    loop {
        match launcher.accept() {
            // The protocol carries stdio and a `Process` handle, so it needs
            // handle transfer. Until that lands the connection is refused by
            // dropping it, which the client reads as a hang-up rather than as
            // a wait.
            Ok(_) => say!("init: launcher: no protocol yet, dropping a client"),
            Err(e) => panic!("init: launcher acceptor refused: {e:?}"),
        }
    }
}

/// Build one program's authority and spawn it holding exactly that.
fn start(
    program: &Program,
    system: &Manifest,
    syscap: &SysCap,
    acceptors: &mut BTreeMap<&str, Acceptor>,
    connectors: &BTreeMap<&str, Connector>,
) {
    let mut command = Command::new(&program.path);
    command.args(&program.args);

    if let Some(ns) = build_namespace(program, system, connectors) {
        command.endow(SVC_LABEL, ns.into_raw().0);
    }

    for name in &program.serves {
        let acceptor = acceptors.remove(name.as_str()).unwrap_or_else(|| {
            // An acceptor is endowed by move, so a `serves` program can be
            // started exactly once per boot. A second start with no acceptor
            // left is refused by name rather than spawned with a hole where
            // its own service should be.
            panic!("init: `{}` has already been given the `{name}` acceptor", program.name)
        });
        command.endow(&format!("{SERVE_PREFIX}{name}"), acceptor.into_raw().0);
    }

    for class in &program.devices {
        let class = DeviceType::from_class_name(class)
            .unwrap_or_else(|| panic!("init: `{class}` is not a device class"));
        // A class no driver registered is not endowed, and init says which:
        // "did I get an HDA or a virtio-sound?" becomes "which claims are in
        // my endowment table?", which is the same question with the answer
        // already in hand.
        match syscap.claim::<toyos::Device>(class) {
            Ok(claim) => {
                command.endow(&format!("{DEV_PREFIX}{}", class.class_name()), claim.into_raw().0);
            }
            Err(e) => say!(
                "init: {}: no {} on this machine ({e:?})",
                program.name,
                class.class_name()
            ),
        }
    }

    if !program.syscap.is_empty() {
        // A duplicate carrying exactly what the manifest asked for and nothing
        // else. Rights only shrink, so soundd's `rt` cap can never mint a claim
        // or open a process however it asks.
        let rights = toyos_manifest::syscap_rights(&program.syscap)
            .unwrap_or_else(|e| panic!("init: {}: {e}", program.name));
        let narrowed = syscap
            .narrowed(rights)
            .expect("init: the system capability refused a narrowed duplicate");
        command.endow(SYSCAP_LABEL, narrowed.into_raw().0);
    }

    match command.spawn() {
        Ok(child) => say!("init: started {} pid={}", program.name, child.id()),
        Err(e) => panic!("init: cannot start {}: {e}", program.name),
    }
}

/// The namespace this program's `receives` names.
///
/// A name some program *provides* rather than serves is not init's to give: it
/// is one port per instance, made by whoever spawns the holder, and it reaches
/// this program from its own parent. A name that is neither is a config the
/// build-time gate should have refused, so it is a panic here.
fn build_namespace(
    program: &Program,
    system: &Manifest,
    connectors: &BTreeMap<&str, Connector>,
) -> Option<Namespace> {
    if program.receives.is_empty() {
        return None;
    }
    let mut builder = namespace::build();
    for name in &program.receives {
        match connectors.get(name.as_str()) {
            Some(connector) => builder = builder.add(name, connector),
            None => assert!(
                system.programs.iter().any(|p| p.provides.contains(name)),
                "init: {} receives `{name}`, which nothing in this image serves or provides",
                program.name,
            ),
        }
    }
    Some(
        builder
            .finish()
            .unwrap_or_else(|e| panic!("init: no namespace for {}: {e:?}", program.name)),
    )
}
