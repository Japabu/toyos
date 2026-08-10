//! What this process was given, and the only place a service name resolves.
//!
//! There is no global registry and no connect-by-name. A process learns what
//! it holds from its own endowment table — `(label, handle)` pairs its parent
//! moved in at spawn — and resolves a service through the namespace in it. A
//! name it was not given resolves to nothing, and there is no second place to
//! ask.
//!
//! **A label is a local name in one process's own table**, so guessing one buys
//! nothing: the table is not enumerable across processes and a name not in
//! yours resolves to nothing wherever it came from.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use toyos_abi::handle::HANDLE_INVALID;
use toyos_abi::syscall::{
    self, DeviceType, SyscallError, MAX_ENDOWMENTS, MAX_LABELS_LEN,
};

use crate::ipc::Connection;
use crate::namespace::Namespace;
use crate::port::{Acceptor, Connector};
use crate::syscap::SysCap;
use crate::{Device, OwnedHandle, RawHandle};

pub use toyos_abi::syscall::{DEV_PREFIX, PROVIDE_PREFIX, SERVE_PREFIX, SVC_LABEL, SYSCAP_LABEL};

/// Why a service name did not become a connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndowError {
    /// The name is not in the namespace this process was given — a fact about
    /// this process, not about the machine. There is no "not yet": every port
    /// exists before either end's process runs.
    NotEndowed,
    /// The server exited. Its acceptor's last handle went with it and the port
    /// is closed for good.
    ServerGone,
    /// The kernel refused for a reason of its own — a full queue, a full table.
    Refused(SyscallError),
}

/// A typed wrapper that can be built from a handle this process already owns.
///
/// The `unsafe` is the whole contract: the caller is asserting that the handle
/// is live, is this process's, and that nothing else answers for it.
pub trait FromHandle {
    /// # Safety
    /// `raw` must be a live handle of the right type that nothing else owns.
    unsafe fn from_handle(raw: RawHandle) -> Self;
}

macro_rules! from_handle {
    ($($ty:ty => $make:expr),+ $(,)?) => {
        $(impl FromHandle for $ty {
            unsafe fn from_handle(raw: RawHandle) -> Self {
                let make: fn(OwnedHandle) -> Self = $make;
                make(OwnedHandle(raw))
            }
        })+
    };
}

from_handle! {
    Acceptor => Acceptor,
    Connector => Connector,
    Namespace => Namespace,
    SysCap => SysCap,
    Device => Device,
    crate::Keyboard => |h| crate::Keyboard(Device(h)),
    crate::Mouse => |h| crate::Mouse(Device(h)),
    crate::FramebufferDev => |h| crate::FramebufferDev(Device(h)),
    crate::Nic => |h| crate::Nic(Device(h)),
    crate::HdaDev => |h| crate::HdaDev(Device(h)),
    crate::VirtioSoundDev => |h| crate::VirtioSoundDev(Device(h)),
}

/// This process's endowment table, parsed once.
pub struct Endowments {
    labels: [u8; MAX_LABELS_LEN],
    /// `(label offset, label length)` per entry, and the handle beside it.
    spans: [(u32, u32); MAX_ENDOWMENTS],
    /// **Taken by swap**, so a label answers exactly once: the second `take`
    /// finds [`HANDLE_INVALID`] and returns `None` rather than a second owner
    /// of one handle.
    handles: [AtomicU32; MAX_ENDOWMENTS],
    count: usize,
}

impl Endowments {
    /// This process's own, parsed on first use.
    pub fn get() -> &'static Endowments {
        TABLE.get()
    }

    /// Take the handle labelled `label`, as `T`. `None` once, for a label this
    /// process was not given; `None` again for one already taken.
    pub fn take<T: FromHandle>(&self, label: &str) -> Option<T> {
        let i = self.index_of(label)?;
        let raw = self.handles[i].swap(HANDLE_INVALID.0, Ordering::AcqRel);
        if raw == HANDLE_INVALID.0 {
            return None;
        }
        // SAFETY: the swap is what makes this the only caller to see the
        // handle, and the kernel put it in this process's table at spawn.
        Some(unsafe { T::from_handle(RawHandle(raw)) })
    }

    /// Whether `label` names something this process still holds.
    pub fn holds(&self, label: &str) -> bool {
        self.index_of(label)
            .is_some_and(|i| self.handles[i].load(Ordering::Acquire) != HANDLE_INVALID.0)
    }

    /// Every label in the table, taken or not. For a diagnostic; nothing
    /// resolves through it.
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        (0..self.count).map(|i| self.label_at(i))
    }

    fn label_at(&self, i: usize) -> &str {
        let (off, len) = self.spans[i];
        let bytes = &self.labels[off as usize..off as usize + len as usize];
        // The kernel copied these bytes out of what this process's own parent
        // wrote, and a label that is not UTF-8 matches no lookup.
        core::str::from_utf8(bytes).unwrap_or("")
    }

    fn index_of(&self, label: &str) -> Option<usize> {
        (0..self.count).find(|&i| self.label_at(i) == label)
    }
}

/// The process's namespace, or `None` for a program the manifest gives no
/// `receives`.
///
/// Borrowed rather than taken: every `service` call resolves through it and a
/// process has exactly one for its whole life.
pub fn namespace() -> Option<&'static Namespace> {
    NAMESPACE.get().as_ref()
}

/// Open a connection to `name` in this process's namespace.
///
/// The one place a name becomes a connection. It works from the caller's first
/// instruction whether or not the server has reached `accept` or has even been
/// spawned, because the port existed before either process did.
pub fn service(name: &str) -> Result<Connection, EndowError> {
    let ns = namespace().ok_or(EndowError::NotEndowed)?;
    ns.open(name).map_err(|e| match e {
        SyscallError::NotFound => EndowError::NotEndowed,
        SyscallError::Gone => EndowError::ServerGone,
        other => EndowError::Refused(other),
    })
}

/// The acceptor of a name the manifest says this program serves.
///
/// A refusal is a build-system bug rather than a race: `src/build.rs` checked
/// the manifest before the image was written, so there is no other process to
/// have taken the name and no name to take.
pub fn acceptor(name: &str) -> Option<Acceptor> {
    with_prefixed(SERVE_PREFIX, name, |label| Endowments::get().take::<Acceptor>(label))
}

/// Every connector a launching client transferred, taken out of the table.
///
/// **What makes a `provides` name forwardable.** A namespace hands back
/// connections; this hands back the connector itself, so a shell can give its
/// own children the surface its terminal gave it — and it is a `take`, so the
/// caller owns exactly one and duplicates for each child.
pub fn provided(labels: &mut [Option<(&'static str, Connector)>]) -> usize {
    let table = Endowments::get();
    let mut n = 0;
    for i in 0..table.count {
        let label = table.label_at(i);
        let Some(name) = label.strip_prefix(PROVIDE_PREFIX) else { continue };
        if n == labels.len() {
            break;
        }
        let Some(connector) = table.take::<Connector>(label) else { continue };
        labels[n] = Some((name, connector));
        n += 1;
    }
    n
}

/// The claim for a device class the manifest says this program gets.
///
/// `None` is a machine that had no such device when init asked, or a program
/// the manifest gives none — the honest answer, and the one soundd degrades
/// on. It replaces a two-syscall probe: "did I get an HDA or a virtio-sound?"
/// is now "which claims are in my endowment table?", which is the same question
/// with the answer already in hand.
pub fn device<T: FromHandle>(class: DeviceType) -> Option<T> {
    with_prefixed(DEV_PREFIX, class.class_name(), |label| Endowments::get().take::<T>(label))
}

/// Compose `<prefix><name>` on the stack. Nothing in the SDK allocates, and a
/// name past the label bound matches nothing rather than being truncated into
/// a label that happens to exist.
fn with_prefixed<T>(prefix: &str, name: &str, f: impl FnOnce(&str) -> Option<T>) -> Option<T> {
    let mut buf = [0u8; 128];
    let total = prefix.len() + name.len();
    if total > buf.len() {
        return None;
    }
    buf[..prefix.len()].copy_from_slice(prefix.as_bytes());
    buf[prefix.len()..total].copy_from_slice(name.as_bytes());
    f(core::str::from_utf8(&buf[..total]).ok()?)
}

/// One value built exactly once, with no allocator and no `Drop`.
///
/// Whatever is in here lives for the process's life, which is what makes
/// handing out a `&'static` to it sound: nothing in the table is ever released
/// while the table exists.
struct Once<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

const UNSET: u8 = 0;
const BUILDING: u8 = 1;
const READY: u8 = 2;

/// The cell is written once, under the `UNSET -> BUILDING` exchange, and read
/// only after `READY`.
unsafe impl<T: Send> Sync for Once<T> {}

impl<T> Once<T> {
    const fn new() -> Self {
        Self { state: AtomicU8::new(UNSET), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    fn get_or_init(&self, build: impl FnOnce() -> T) -> &T {
        if self
            .state
            .compare_exchange(UNSET, BUILDING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // SAFETY: the exchange makes this thread the only writer, and no
            // reader proceeds before `READY` is stored below.
            unsafe { (*self.value.get()).write(build()) };
            self.state.store(READY, Ordering::Release);
        }
        while self.state.load(Ordering::Acquire) != READY {
            core::hint::spin_loop();
        }
        // SAFETY: `READY` is stored after the write, with release/acquire.
        unsafe { (*self.value.get()).assume_init_ref() }
    }
}

static TABLE: EndowTable = EndowTable(Once::new());
static NAMESPACE: NamespaceCell = NamespaceCell(Once::new());

struct EndowTable(Once<Endowments>);
struct NamespaceCell(Once<Option<Namespace>>);

impl EndowTable {
    fn get(&'static self) -> &'static Endowments {
        self.0.get_or_init(read_table)
    }
}

impl NamespaceCell {
    fn get(&'static self) -> &'static Option<Namespace> {
        self.0.get_or_init(|| Endowments::get().take::<Namespace>(SVC_LABEL))
    }
}

/// One `SYS_ENDOWMENTS` call: a count, that many entries, then the label blob.
fn read_table() -> Endowments {
    const ENTRY: usize = 16;
    let mut table = Endowments {
        labels: [0; MAX_LABELS_LEN],
        spans: [(0, 0); MAX_ENDOWMENTS],
        handles: [const { AtomicU32::new(HANDLE_INVALID.0) }; MAX_ENDOWMENTS],
        count: 0,
    };
    let mut buf = [0u8; 8 + MAX_ENDOWMENTS * ENTRY + MAX_LABELS_LEN];
    let n = syscall::endowments(&mut buf);
    if n < 8 || n > buf.len() {
        return table;
    }
    let count = u64::from_ne_bytes(buf[..8].try_into().unwrap()) as usize;
    // The kernel bounds this at spawn; a count past it here is a kernel bug,
    // and answering with an empty table would hide it. Fail-fast.
    assert!(count <= MAX_ENDOWMENTS, "the kernel reported {count} endowments");
    let blob = 8 + count * ENTRY;
    assert!(blob <= n, "the endowment table claims more entries than it carries");
    let labels = &buf[blob..n];
    table.labels[..labels.len()].copy_from_slice(labels);
    for i in 0..count {
        let at = 8 + i * ENTRY;
        let off = u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap());
        let len = u32::from_ne_bytes(buf[at + 4..at + 8].try_into().unwrap());
        let handle = u32::from_ne_bytes(buf[at + 8..at + 12].try_into().unwrap());
        table.spans[i] = (off, len);
        table.handles[i].store(handle, Ordering::Relaxed);
    }
    table.count = count;
    table
}
