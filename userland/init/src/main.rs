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
use std::os::toyos::process::{ChildExt, CommandExt};
use std::process::{Child, Command};

use toyos_manifest::{Manifest, Program};
use toyos::endow::Endowments;
use toyos::ipc::Connection;
use toyos::launch::{self, Request};
use toyos::namespace::{self, Namespace};
use toyos::port::{self, Acceptor, Connector};
use toyos::syscap::SysCap;
use toyos::AsHandle;
use toyos_abi::syscall::{
    DeviceType, DEV_PREFIX, PROVIDE_PREFIX, SERVE_PREFIX, SVC_LABEL, SYSCAP_LABEL,
};

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

    // Kept for the machine's life: init is the only thing that can kill a
    // daemon, and there is no other way back to a process it started.
    let mut booted: Vec<Child> = Vec::new();
    for name in &system.start {
        let program = system
            .program(name)
            .unwrap_or_else(|| panic!("init: [boot] start names `{name}`, which is not declared"));
        match start(
            Command::new(&program.path),
            program,
            &system,
            &syscap,
            &mut acceptors,
            &connectors,
            &[],
        ) {
            Ok(child) => booted.push(child),
            Err(e) => panic!("init: cannot start {}: {e}", program.name),
        }
    }

    // Nothing else holds a `serves` acceptor that has not been launched yet, so
    // init outliving its children is what keeps those ports open. It parks
    // here.
    let launcher = acceptors
        .remove(LAUNCHER)
        .expect("init: the manifest declares init serves `launcher`");
    loop {
        match launcher.accept() {
            Ok(conn) => serve_launch(&conn, &system, &syscap, &mut acceptors, &connectors),
            Err(e) => panic!("init: launcher acceptor refused: {e:?}"),
        }
    }
}

/// One `MSG_LAUNCH`, from the frame to the `Process` handle that answers it.
///
/// **Everything in the request is a client's claim about itself.** A program
/// nothing declares is refused by name; a frame that does not decode is a
/// dropped connection and nothing else. What the child ends up holding is the
/// manifest's row for it plus whatever connectors the caller transferred, and
/// the caller could only transfer what it already had — so a launch confers
/// exactly the manifest row and nothing beyond it.
fn serve_launch<'a>(
    conn: &Connection,
    system: &'a Manifest,
    syscap: &SysCap,
    acceptors: &mut BTreeMap<&'a str, Acceptor>,
    connectors: &BTreeMap<&str, Connector>,
) {
    let Ok(header) = conn.recv_header() else { return };
    if header.msg_type != launch::MSG_LAUNCH {
        return;
    }
    let mut buf = [0u8; toyos::ipc::MAX_FRAME_LEN as usize];
    let Ok(len) = conn.recv_bytes(&header, &mut buf) else { return };
    let mut batch = [toyos::RawHandle(0); toyos_abi::syscall::MAX_TRANSFER_HANDLES];
    let received = conn.recv_handles(&mut batch).unwrap_or(0);

    // **Owned on the statement after they arrive, and before anything can
    // refuse.** The send moved them into init's table, so every path out of
    // here releases them — a launcher that leaked a handle per refused launch
    // would exhaust the one table the machine cannot do without, and a client
    // picks which refusal it takes.
    let mut held = Moved(batch[..received].to_vec());

    let Some(request) = Request::decode(&buf[..len]) else { return };
    // `extra_names` drops an empty or non-UTF-8 name, so its count is what
    // will actually be paired with a handle. A frame whose two counts
    // disagree would otherwise leave the unpaired handles behind.
    let names: Vec<&str> = request.extra_names().collect();
    if received != request.slot_count() + request.extra_count
        || names.len() != request.extra_count
    {
        say!(
            "init: launcher: a frame promising {} handles under {} names carried {received}",
            request.slot_count() + request.extra_count,
            names.len(),
        );
        return;
    }

    // Past every refusal that does not know which handle is which, so ownership
    // can be split. Both halves still release on every path below.
    let all = held.take();
    let (slot_handles, extra_handles) = all.split_at(request.slot_count());
    let slots = Moved(slot_handles.to_vec());
    // Owned, so they close when this call returns: `SYS_NAMESPACE_BUILD` copies
    // a connector into the namespace and leaves the caller's handle, and init's
    // copy of a client's connector has no life beyond this launch.
    let extras: Vec<(&str, Connector)> = names
        .into_iter()
        .zip(extra_handles.iter().copied())
        // SAFETY: the kernel moved these into init's table with the frame, and
        // nothing else answers for them. **Not a claim about the type** — a
        // client sends what it likes, and everything below treats a wrong one
        // as a refused launch rather than as init's own bug.
        .map(|(name, handle)| (name, unsafe { Connector::from_raw(handle) }))
        .collect();

    let Some(program) = declared(system, request.program) else {
        let _ = conn.send_bytes(launch::MSG_NOT_DECLARED, &[]);
        return;
    };

    // **The caller's path, not the row's, and `argv[0]` is why.** `declared`
    // has already established that the two name one binary, so this grants
    // nothing extra — and `/bin/echo` spawned as `/bin/toybox` is a toybox that
    // was never told which applet it is.
    let mut command = Command::new(request.program);
    for (slot, handle) in request.slot_numbers().zip(slots.0.iter().copied()) {
        command.inherit_fd(slot, handle.0);
    }
    // **Carried, not inherited.** A child of the launcher would otherwise get
    // init's environment and init's working directory, so `cd /tmp && ls` would
    // list `/`. The launcher is a spawn service, not a session.
    command.env_clear();
    for entry in request.env.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        let Some(eq) = entry.iter().position(|&b| b == b'=') else { continue };
        if let (Ok(key), Ok(value)) =
            (std::str::from_utf8(&entry[..eq]), std::str::from_utf8(&entry[eq + 1..]))
        {
            command.env(key, value);
        }
    }
    if !request.cwd.is_empty() {
        command.current_dir(request.cwd);
    }
    for arg in request.argv.split(|&b| b == 0).skip(1).filter(|a| !a.is_empty()) {
        if let Ok(arg) = std::str::from_utf8(arg) {
            command.arg(arg);
        }
    }

    // `inherit_fd` duplicates into the child, so init's own copies go with
    // `slots` when this returns.
    let started = start(command, program, system, syscap, acceptors, connectors, &extras);
    match started {
        Ok(child) => {
            let handle = toyos::RawHandle(child.into_raw_handle());
            if conn
                .send_bytes_with_handles(&[handle], launch::MSG_LAUNCHED, &[])
                .is_err()
            {
                // **init keeps no `Process` handle from a launch.** A send that
                // did not happen leaves it holding one, and a client that could
                // do that in a loop would exhaust the one handle table the whole
                // machine depends on.
                toyos_abi::syscall::close(handle);
            }
        }
        Err(e) => {
            say!("init: launcher: cannot start {}: {e}", program.name);
            let _ = conn.send_bytes(launch::MSG_REFUSED, &[]);
        }
    }
}

/// Handles a launch moved into init, released on every path out of it.
///
/// A `Drop` and not a close at each `return`: there are seven ways out of
/// `serve_launch` and a client picks which one by what it sends.
struct Moved(Vec<toyos::RawHandle>);

impl Moved {
    /// Give up the obligation, for a caller taking it on itself.
    fn take(&mut self) -> Vec<toyos::RawHandle> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for Moved {
    fn drop(&mut self) {
        for handle in &self.0 {
            toyos_abi::syscall::close(*handle);
        }
    }
}

/// The `[programs]` row a launch's path names, following one symlink.
///
/// `/bin/ls` is a symlink to `/bin/toybox`, and what an applet may hold is
/// `toybox`'s row: the granularity of least authority is the granularity of the
/// binary.
///
/// **The row's own path has to match, and matching it is the whole check.** The
/// path is a client's claim and the row is authority: keyed on the basename
/// alone, a caller writing `/tmp/toybox` would be handed `toybox`'s namespace
/// for a binary it wrote itself. So the answer is a row only where the caller
/// named that row's binary — directly, or through a link that lands on it.
fn declared<'a>(system: &'a Manifest, path: &str) -> Option<&'a Program> {
    let key = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let row = |p: &str| system.program(&key(p)).filter(|program| program.path == p);
    if let Some(program) = row(path) {
        return Some(program);
    }
    let target = std::fs::read_link(path).ok()?;
    row(target.to_str()?)
}

/// Build one program's authority and spawn it holding exactly that.
///
/// `extras` are connectors a launching client transferred: names the manifest
/// cannot know because there is one port per instance of whoever made them —
/// a terminal's `surface`. They are added *to* the manifest's row rather than
/// replacing it, and a caller could only transfer what it already held, so a
/// launch confers the row and nothing beyond it.
fn start<'a>(
    mut command: Command,
    program: &'a Program,
    system: &Manifest,
    syscap: &SysCap,
    acceptors: &mut BTreeMap<&'a str, Acceptor>,
    connectors: &BTreeMap<&str, Connector>,
    extras: &[(&str, Connector)],
) -> std::io::Result<Child> {
    command.args(&program.args);

    // **Everything endowed stays owned until the spawn that moves it
    // succeeds.** `endow` records a number; a refused spawn moves nothing
    // (`build_child_handles` is all-or-nothing), so a handle whose owner had
    // already been forgotten would be one init holds for ever and a device
    // class nothing can mint again. A client picks whether a spawn fails — an
    // unreachable `cwd` is enough — so this is its path as much as a bug's.
    let mut held = Moved(Vec::new());
    // Acceptors are given *back* rather than closed: a `serves` port whose
    // acceptor is gone can never be served again.
    let mut taken: Vec<(&'a str, Acceptor)> = Vec::new();

    if let Some(ns) = build_namespace(program, system, connectors, extras)? {
        let raw = ns.into_raw();
        command.endow(SVC_LABEL, raw.0);
        held.0.push(raw);
    }

    // The namespace answers with connections, so a child holding `surface`
    // only there could never hand `surface` to a child of its own — which is
    // the terminal → shell → `locale` chain. The connector travels labelled as
    // well, and a duplicate because the namespace above took one of its own.
    for (name, connector) in extras {
        // **Not an `expect`.** The connector is a client's, and a client may
        // send one it narrowed `DUP` away from. That is a refused launch, not
        // init's bug — and init is the one process the machine cannot lose.
        let Ok(handed) = connector.duplicate() else {
            return Err(std::io::Error::other("a provided connector cannot be duplicated"));
        };
        let raw = handed.into_raw();
        command.endow(&format!("{PROVIDE_PREFIX}{name}"), raw.0);
        held.0.push(raw);
    }

    for name in &program.serves {
        let acceptor = acceptors.remove(name.as_str()).unwrap_or_else(|| {
            // An acceptor is endowed by move, so a `serves` program can be
            // started exactly once per boot. A second start with no acceptor
            // left is refused by name rather than spawned with a hole where
            // its own service should be.
            panic!("init: `{}` has already been given the `{name}` acceptor", program.name)
        });
        command.endow(&format!("{SERVE_PREFIX}{name}"), acceptor.as_handle().0);
        taken.push((name.as_str(), acceptor));
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
                let raw = claim.into_raw();
                command.endow(&format!("{DEV_PREFIX}{}", class.class_name()), raw.0);
                held.0.push(raw);
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
        let raw = narrowed.into_raw();
        command.endow(SYSCAP_LABEL, raw.0);
        held.0.push(raw);
    }

    match command.spawn() {
        Ok(child) => {
            // The spawn moved every one of them into the child's table, so
            // nothing here may close them.
            held.take();
            for (_, acceptor) in taken {
                let _ = acceptor.into_raw();
            }
            say!("init: started {}", program.name);
            Ok(child)
        }
        Err(e) => {
            for (name, acceptor) in taken {
                acceptors.insert(name, acceptor);
            }
            Err(e)
        }
    }
}

/// The namespace this program's `receives` names.
///
/// A name some program *provides* rather than serves is not init's to give: it
/// is one port per instance, made by whoever spawns the holder, and it reaches
/// this program from its own parent. A name that is neither is a config the
/// build-time gate should have refused, so it is a panic here.
/// A refusal is a launch that does not happen, never a panic: `extras` are a
/// client's handles and the kernel judges their type, so `finish` answers
/// `InvalidArgument` for a client that sent a pipe where a connector belongs.
fn build_namespace(
    program: &Program,
    system: &Manifest,
    connectors: &BTreeMap<&str, Connector>,
    extras: &[(&str, Connector)],
) -> std::io::Result<Option<Namespace>> {
    if program.receives.is_empty() && extras.is_empty() {
        return Ok(None);
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
    // A `provides` name reaches its holder from whoever made the port, never
    // from init, and this is where it arrives.
    for (name, connector) in extras {
        builder = builder.add(name, connector);
    }
    match builder.finish() {
        Ok(ns) => Ok(Some(ns)),
        Err(e) if extras.is_empty() => {
            panic!("init: no namespace for {}: {e:?}", program.name)
        }
        Err(e) => Err(std::io::Error::other(format!("a provided connector was refused: {e:?}"))),
    }
}
