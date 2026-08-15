//! Kernel objects, and the identity every one of them carries.
//!
//! Every object is a plain `Arc<T>`. There is no custom refcounting, no `Weak`
//! anywhere in the graph, and no `dyn` hierarchy: `Arc` already proves
//! drop-exactly-once-after-the-last-reference, and hand-rolled counting is the
//! bug class this layer exists to delete.
//!
//! **`handle_count` is not the Arc count, and that is load-bearing.** This
//! kernel does not unwind, so an `Arc` a syscall cloned out of a table before
//! blocking is stranded on the kernel stack of a thread another CPU killed. If
//! EOF and dead-peer detection rode Arc counts, killing an audio client blocked
//! in its signal-pipe read — the steady state of every cpal client — would
//! leak the read end and soundd would never learn. So every *userland-visible*
//! lifecycle event rides `handle_count`, which process teardown drains on the
//! killer's CPU. The stranded `Arc` leaks memory: bounded, visible in the
//! [`census`], and unable to delay a semantic event.
//!
//! `specs/capability-endowment-spec.md` §2.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::num::NonZeroU64;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::sync::Lock;

pub mod device;
pub mod file;
pub mod handle;
pub mod namespace;
pub mod ops;
pub mod pipe;
pub mod port;
pub mod process;
pub mod service;
pub mod shm;
pub mod syscap;

pub use handle::{HandleEntry, HandleError, HandleTable, Refusal};

/// A resource an object owns and must give back when its **last handle** goes.
///
/// A plain field gives it back when the last `Arc` goes, and an `Arc` stranded
/// on a killed thread's kernel stack would hold it forever — a writer that
/// never closes is a reader that never sees EOF, which is exactly the audio
/// client this layer exists to get right. `on_zero_handles` takes it instead,
/// from the deferred queue with nothing held.
pub(crate) struct Held<T>(Lock<Option<T>>);

impl<T> Held<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(Lock::new(Some(value)))
    }

    /// Drop what is held, outside the lock: whatever `T`'s destructor reaches
    /// for — `PIPES`, the listener registry, a device owner — must not be taken
    /// under this one.
    pub(crate) fn release(&self) {
        let taken = self.0.lock().take();
        drop(taken);
    }
}

impl<T: Clone> Held<T> {
    /// A second counted reference to the same thing, or `None` once the last
    /// handle gave it back.
    pub(crate) fn get(&self) -> Option<T> {
        self.0.lock().clone()
    }
}

/// A kernel object's identity.
///
/// For diagnostics and for kernel-internal keys — an io_uring watch names the
/// object it watches by this, because an fd number means nothing in another
/// process's table. **Never an authority**: no syscall turns a koid into
/// access to anything.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Koid(NonZeroU64);

impl Koid {
    pub fn raw(self) -> u64 {
        self.0.get()
    }

    fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let raw = NEXT.fetch_add(1, Ordering::Relaxed);
        Self(NonZeroU64::new(raw).expect("koid counter wrapped through zero"))
    }
}

/// What every object embeds.
///
/// Constructed only by the per-type `core()` the [`kobject!`] macro generates,
/// so an object that is not in the census cannot be built.
pub struct ObjectCore {
    koid: Koid,
    /// Table slots, in-flight transfers and spawn endowments — never the `Arc`
    /// strong count. Reaching zero fires [`KObjectVariant::on_zero_handles`]
    /// exactly once.
    handle_count: AtomicU32,
    /// Set the first time the count reaches zero. A second arrival is a kernel
    /// bug, and the assert in `HandleEntry`'s drop is where it is caught.
    retired: AtomicBool,
    /// This type's census counter, so the decrement rides the core's own drop
    /// and an object type stays free to write its own [`Drop`].
    live: &'static AtomicU64,
}

impl ObjectCore {
    pub fn koid(&self) -> Koid {
        self.koid
    }

    pub fn handle_count(&self) -> u32 {
        self.handle_count.load(Ordering::Acquire)
    }

    pub fn retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    fn enrol(live: &'static AtomicU64) -> Self {
        live.fetch_add(1, Ordering::Relaxed);
        Self {
            koid: Koid::mint(),
            handle_count: AtomicU32::new(0),
            retired: AtomicBool::new(false),
            live,
        }
    }
}

impl Drop for ObjectCore {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What an object does when its last handle goes.
///
/// Separate from [`KObjectVariant`], which the macro writes: a `deferred` row
/// must write this one itself and the method has no default body, so adding a
/// type does not compile until somebody has said what its last handle means.
/// An `immediate` row gets an empty impl from the macro, and a hand-written one
/// beside it is a coherence error — so "a hook that is never run" is not a
/// state this module can be left in.
pub trait ZeroHandles {
    /// Runs exactly once, from the deferred queue, with no lock held. Never
    /// inline at drop time — see [`drain_zero_handles`].
    fn on_zero_handles(&self);
}

/// One object type's plumbing.
///
/// Implemented by the [`kobject!`] macro and by nothing else, so a new object
/// type is one macro row and a compile error at every exhaustive match.
pub trait KObjectVariant: ZeroHandles + Send + Sync + Sized + 'static {
    const NAME: &'static str;
    fn from_ref(r: &KObjectRef) -> Option<&Arc<Self>>;
    fn into_ref(this: Arc<Self>) -> KObjectRef;
    fn core(&self) -> &ObjectCore;
    /// A fresh core, enrolled in this type's census. The only way to build one.
    fn new_core() -> ObjectCore;
}

/// Declare the closed set of object types.
///
/// One row per type generates the `KObjectRef` variant, the `KObjectVariant`
/// impl and a live census counter. Object-layer dispatch matches this enum
/// exhaustively with no `_` arms, so adding a row is a compile error wherever a
/// decision has to be made about it.
///
/// **Each row says whether its last handle is an event.** A `deferred` row has
/// a [`ZeroHandles`] hook and is released from the queue, with nothing held. An
/// `immediate` row has none, so there is nothing to defer and the last `Arc`
/// goes where the handle did — which is where it went before this layer
/// existed, and which is what keeps a killed process's file flush on the
/// killer's 128 KiB kernel stack instead of a 16 KiB idle one.
macro_rules! kobject {
    ($($kind:ident $variant:ident => $ty:ty),+ $(,)?) => {
        /// Every kind of thing a handle can name.
        #[derive(Clone)]
        pub enum KObjectRef {
            $($variant(Arc<$ty>),)+
        }

        impl KObjectRef {
            pub fn core(&self) -> &ObjectCore {
                match self {
                    $(Self::$variant(o) => o.core(),)+
                }
            }

            /// This object's type, for a `WrongType` refusal that names both.
            pub fn kind(&self) -> &'static str {
                match self {
                    $(Self::$variant(_) => <$ty as KObjectVariant>::NAME,)+
                }
            }

            fn run_zero_handles(&self) {
                match self {
                    $(Self::$variant(o) => o.on_zero_handles(),)+
                }
            }

            /// Whether releasing this object waits for the deferred queue.
            fn defers_release(&self) -> bool {
                match self {
                    $(Self::$variant(_) => kobject!(@defers $kind),)+
                }
            }
        }

        $(
            kobject!(@empty_hook $kind $ty);

            impl KObjectVariant for $ty {
                const NAME: &'static str = stringify!($variant);

                fn from_ref(r: &KObjectRef) -> Option<&Arc<Self>> {
                    match r {
                        KObjectRef::$variant(o) => Some(o),
                        // Reachable for every variant but the first, and the
                        // macro cannot know how many rows follow it.
                        #[allow(unreachable_patterns)]
                        _ => None,
                    }
                }

                fn into_ref(this: Arc<Self>) -> KObjectRef {
                    KObjectRef::$variant(this)
                }

                fn core(&self) -> &ObjectCore {
                    &self.core
                }

                fn new_core() -> ObjectCore {
                    ObjectCore::enrol(&census::$variant)
                }
            }
        )+

        /// How many objects of each type are alive right now.
        ///
        /// The detector for the two leaks this design accepts — an `Arc`
        /// stranded on a killed thread's stack, and the cross-pair connection
        /// cycle — because neither is visible any other way. A churn test
        /// asserts these return to the baseline they started from.
        pub mod census {
            use core::sync::atomic::{AtomicU64, Ordering};

            $(
                #[allow(non_upper_case_globals)]
                pub(super) static $variant: AtomicU64 = AtomicU64::new(0);
            )+

            /// `(type name, live count)`, in declaration order.
            ///
            /// Per kind and only per kind. The machine-wide sum this used to
            /// offer beside it hid a leak of one kind behind ordinary churn in
            /// another, and once every leak assertion in the estate was per
            /// kind it had no reader left.
            pub fn live() -> impl Iterator<Item = (&'static str, u64)> {
                [$((stringify!($variant), $variant.load(Ordering::Relaxed)),)+].into_iter()
            }
        }
    };

    (@defers deferred) => { true };
    (@defers immediate) => { false };

    (@empty_hook deferred $ty:ty) => {};
    (@empty_hook immediate $ty:ty) => {
        impl ZeroHandles for $ty {
            fn on_zero_handles(&self) {}
        }
    };
}

kobject! {
    deferred PipeRead => pipe::PipeReadEnd,
    deferred PipeWrite => pipe::PipeWriteEnd,
    deferred Connection => service::ConnectionEnd,
    deferred Device => device::DeviceClaim,
    deferred Acceptor => port::Acceptor,
    deferred IoUring => service::IoUringObject,
    // Every mapping goes with the last handle, and the flush that makes that
    // safe waits for every other CPU — so it runs from the queue, with nothing
    // held, and never inline at a `close`.
    deferred SharedMem => shm::SharedMemObject,
    // A service with no clients right now is not a service that has stopped.
    immediate Connector => port::Connector,
    // Immutable once built: its `Arc<Connector>`s go with the last reference
    // and nothing observes it.
    immediate Namespace => namespace::Namespace,
    // A file's flush and its cache reference ride the last `Arc`, and no
    // blocking syscall strands one: `read` and `write` on a file never park.
    immediate File => file::FileObject,
    immediate Console => device::ConsoleObject,
    // The authority is the rights on the handle, so a handle going away *is*
    // the whole event.
    immediate SysCap => syscap::SysCap,
    // A process nobody holds a handle to is not a process that should stop. The
    // last handle going is the loss of the *ability to wait*, and the object
    // outlives it only for as long as the table entry does.
    immediate Process => process::ProcessObject,
}

/// Objects whose last handle has gone, waiting for a context with nothing held.
///
/// **A hook can never run under a lock, and that is structural rather than a
/// discipline rule.** `HandleEntry`'s drop pushes here; nothing calls
/// `on_zero_handles` inline. So the `drop(pd.lock().handles.remove(h)?)`
/// temporary-lifetime trap — the guard outliving the drop — cannot run
/// subsystem code however carelessly a call site is written, and a cascade
/// (a connection drops queued entries which drop a device claim…) is a queue
/// iteration rather than recursion.
///
/// One machine-wide queue rather than per-CPU: it is touched once per handle
/// release, which is nowhere near a hot path, and a per-CPU queue would strand
/// work on a CPU that then halts.
static ZERO_QUEUE: Lock<Vec<KObjectRef>> = Lock::new(Vec::new());

/// Whether [`ZERO_QUEUE`] holds anything, readable without taking it.
///
/// The drain runs at every syscall exit and every scheduler pass, and
/// `Lock::lock` is a `fetch_add` — the one operation TCG cannot emit inline,
/// and a few hundred a boot of it cost 350 ms of boot
/// (`specs/issues/hardware/one-rmw-per-log-line-cost-350ms.md`). Written under the lock at
/// both ends, so it never says "empty" over a queued object; a stale
/// "non-empty" costs one drain that finds nothing.
static ZERO_PENDING: AtomicBool = AtomicBool::new(false);

pub(crate) fn enqueue_zero_handles(object: KObjectRef) {
    let mut queue = ZERO_QUEUE.lock();
    queue.push(object);
    ZERO_PENDING.store(true, Ordering::Release);
}

/// Run the hooks of every object whose last handle has gone.
///
/// Called at syscall exit, at the top of every scheduler pass and from the idle
/// loop — the same three sites, and for the same reason, as the wake drains
/// beside them. Latency is one of those, which is microseconds; nothing here
/// waits on anything.
pub fn drain_zero_handles() {
    while ZERO_PENDING.load(Ordering::Acquire) {
        // A hook may retire further objects — dropping a connection drops the
        // handles queued in it — so this repeats until the queue stays empty.
        let batch = {
            let mut queue = ZERO_QUEUE.lock();
            ZERO_PENDING.store(false, Ordering::Release);
            core::mem::take(&mut *queue)
        };
        for object in batch {
            object.run_zero_handles();
        }
    }
}
