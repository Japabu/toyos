//! A device claim, and the one console every kernel-spawned process starts on.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use toyos_abi::handle::{RawHandle, Rights};
use toyos_abi::syscall::SyscallError;
use toyos_abi::FramebufferInfo;

use crate::device::{Claim, DeviceType};

use super::handle::{HandleEntry, HandleTable};
use super::shm::SharedMemObject;
use super::{Held, KObjectRef, KObjectVariant, ObjectCore, ZeroHandles};

/// What the class answers when its holder reads it.
///
/// Keyboard and mouse answer with events rather than with a description, which
/// is why they have no arm here rather than an empty one.
///
/// **Every buffer a description names travels beside it as an object**, and the
/// handle fields in the wire struct are filled in by the read that answers.
/// They used to be shared-memory tokens the kernel granted to the reading pid,
/// which is the last place in the system where a number crossed the boundary
/// and stood for authority.
pub enum DeviceInfo {
    Events,
    Framebuffer(FramebufferInfo, FramebufferBuffers),
    Nic(crate::net::NicInfo, Arc<SharedMemObject>),
    Hda(toyos_abi::hda::HdaInfo, Arc<SharedMemObject>),
    VirtioSound(toyos_abi::virtio_sound::VirtioSoundInfo, Arc<SharedMemObject>),
}

/// The two scanout buffers and the cursor plane.
pub struct FramebufferBuffers {
    pub scanout: [Arc<SharedMemObject>; 2],
    pub cursor: Arc<SharedMemObject>,
}

/// What a buffer handle in a device description carries.
///
/// `MAP` is the point of it. `DUP` and `TRANSFER` because a daemon may hand a
/// buffer on — soundd's mixer thread and the compositor's panel both live in
/// the same process today, but nothing here should be the reason that cannot
/// change.
const BUFFER_RIGHTS: Rights = Rights::MAP.union(Rights::DUP).union(Rights::TRANSFER);

fn install_buffer(
    table: &mut HandleTable,
    buffer: &Arc<SharedMemObject>,
) -> Result<RawHandle, SyscallError> {
    table
        .install(HandleEntry::new(KObjectRef::SharedMem(buffer.clone()), BUFFER_RIGHTS))
        .map_err(|_| SyscallError::ResourceExhausted)
}

impl DeviceInfo {
    /// The description as bytes, with a handle installed for every buffer it
    /// names.
    fn mint(&self, table: &mut HandleTable) -> Result<Box<[u8]>, SyscallError> {
        Ok(match self {
            Self::Events => Box::new([]),
            Self::Framebuffer(info, buffers) => {
                let mut info = *info;
                info.scanout = [
                    install_buffer(table, &buffers.scanout[0])?,
                    install_buffer(table, &buffers.scanout[1])?,
                ];
                info.cursor = install_buffer(table, &buffers.cursor)?;
                info.as_bytes().into()
            }
            Self::Nic(info, dma) => {
                let mut info = *info;
                info.dma = install_buffer(table, dma)?;
                info.as_bytes().into()
            }
            Self::Hda(info, pcm) => {
                let mut info = *info;
                info.pcm = install_buffer(table, pcm)?;
                info.as_bytes().into()
            }
            Self::VirtioSound(info, dma) => {
                let mut info = *info;
                info.dma = install_buffer(table, dma)?;
                info.as_bytes().into()
            }
        })
    }
}

/// One process's exclusive hold on a device class.
///
/// Created **without `Rights::DUP`**, so at most one handle to a claim can
/// exist and a transfer is a move. That is what makes `info_read` — "has the
/// holder taken the description yet?" — sound on the object rather than per
/// handle: there is no second handle to disagree with it.
pub struct DeviceClaim {
    pub(super) core: ObjectCore,
    class: DeviceType,
    info_read: AtomicBool,
    described: crate::sync::Lock<Described>,
    reference: Held<Claim>,
}

/// What this claim describes, and the wire image its holder has been given.
///
/// **The image is minted once.** Its handle fields name slots in *this*
/// process's table, so re-minting on a second read would install a second
/// handle to the same buffer every time — an unbounded handle leak a process
/// could drive by reading in a loop. A claim admits one handle, so there is
/// exactly one holder to mint for and one answer to remember.
///
/// The description is behind the same lock because a mode set replaces both at
/// once: `SYS_GPU_SET_RESOLUTION` reallocates the scanout, so the buffers a
/// later read should name are not the ones the claim was minted with.
struct Described {
    info: DeviceInfo,
    bytes: Option<Box<[u8]>>,
}

impl DeviceClaim {
    pub fn new(class: DeviceType, info: DeviceInfo, claim: Claim) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            class,
            info_read: AtomicBool::new(false),
            described: crate::sync::Lock::new(Described { info, bytes: None }),
            reference: Held::new(claim),
        })
    }

    pub fn class(&self) -> DeviceType {
        self.class
    }

    pub fn info_read(&self) -> bool {
        self.info_read.load(Ordering::Relaxed)
    }

    /// Write this claim's description into `buf`, installing a handle for every
    /// buffer it names the first time and answering with the same numbers
    /// after.
    pub fn describe(
        &self,
        table: &mut HandleTable,
        buf: &mut crate::user_ptr::UserBytesMut,
    ) -> u64 {
        let mut described = self.described.lock();
        if described.bytes.is_none() {
            match described.info.mint(table) {
                Ok(minted) => described.bytes = Some(minted),
                Err(e) => return e.to_u64(),
            }
        }
        let bytes = described.bytes.as_deref().expect("just minted");
        let count = buf.len().min(bytes.len());
        buf.write_at(0, &bytes[..count]);
        self.info_read.store(true, Ordering::Relaxed);
        count as u64
    }

    /// Replace the description, for a mode set that reallocated the buffers.
    ///
    /// The handles this hands back are fresh; the ones the previous description
    /// named keep working until their holder closes them, which is what lets a
    /// compositor keep blitting the old scanout until it has mapped the new
    /// one. Nothing is revoked — the pages are the old object's and go with it.
    pub fn remint(
        &self,
        table: &mut HandleTable,
        info: DeviceInfo,
    ) -> Result<Box<[u8]>, SyscallError> {
        let mut described = self.described.lock();
        let minted = info.mint(table)?;
        described.info = info;
        described.bytes = Some(minted.clone());
        Ok(minted)
    }
}

/// The claim goes back when the last *handle* does, not when the last `Arc`
/// does: a daemon killed while parked on its device — soundd's steady state —
/// strands an `Arc` on a freed kernel stack, and a claim released by Arc count
/// would then never come back for the process that replaces it.
impl ZeroHandles for DeviceClaim {
    fn on_zero_handles(&self) {
        self.reference.release();
    }
}

/// One holder's view of the machine's serial console.
///
/// Not a [`DeviceClaim`] — a claim's whole content is exclusivity — and not a
/// file: it has no path, no cursor and no backing. What it *is* is the line
/// buffer in front of one backend, which is where console line atomicity comes
/// from.
///
/// **One backend, one object per holder, and the second half is load-bearing.**
/// A buffer on one object every process shares is one buffer two processes
/// accumulate into, and their two half-lines splice inside the very mechanism
/// that exists to stop splicing. So `ConsoleObject::new()` is called once by
/// `spawn_init` and once per inherited console slot in
/// `loader::start::build_child_handles`: a child gets its own object over the
/// one backend rather than a duplicate of its parent's handle. Authority is
/// unchanged by that — a process has a console exactly when its parent gave it
/// one, which is the rule the slot map already expressed — and the panic path
/// depends on none of them, because `panic_flush` writes the backend directly.
pub struct ConsoleObject {
    pub(super) core: ObjectCore,
    /// Bytes written but not yet ended by a newline.
    ///
    /// **Not a leaf, and this comment used to say it was.** `ConsoleLine::write`
    /// flushes every whole line it completes, and `Stripped::flush` takes
    /// `serial::BackendGuard` — so the backend's spinlock is held *underneath*
    /// this one, once per line, for as long as the device write takes.
    ///
    /// The order is **`process_data` → `line` → `BackendGuard`**, and it is
    /// consistent because nothing takes those three in any other order:
    ///
    /// - Only two things take `line`: this type's `write`, reached from
    ///   `ops::try_write` under the writing process's own `process_data`, and
    ///   its `Drop`, which runs where the last handle did — also under that
    ///   lock, and holding nothing below it yet.
    /// - Nothing that holds `BackendGuard` takes `line` or `process_data`. The
    ///   other three producers write the *backend* and never an object:
    ///   `klogd`'s drain, `panic_flush`/`flush_final`, and the input path. That
    ///   is §4.1's split, and it is what makes a console object something a
    ///   dying machine does not need.
    /// - `BackendGuard` is `cli` plus a global spin, so what is bounded under it
    ///   is one `MAX_CONSOLE_LINE` and never a userland length — the reason
    ///   `ConsoleLine` cuts a long line into pieces at all.
    line: crate::sync::Lock<crate::drivers::serial::ConsoleLine>,
}

impl ConsoleObject {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            line: crate::sync::Lock::new(crate::drivers::serial::ConsoleLine::new()),
        })
    }

    /// Take a userland write, emitting every whole line it completes.
    pub fn write(&self, buf: &crate::user_ptr::UserBytes) {
        // The negative control, and it is literally the state this tree shipped
        // between L3 and L5: every `write` reaches the backend as it arrives,
        // so a `println!` hands the kernel half a line and a kernel record can
        // land in the gap. `console_line_atomicity` reds under it.
        if crate::actuator::console_unbuffered() {
            crate::drivers::serial::write_console(buf);
            return;
        }
        self.line.lock().write(buf);
    }
}

/// The last handle going is the one moment a partial line stops being
/// unfinished and becomes all there will ever be.
///
/// A process that exits mid-line said those bytes; a buffer that dropped them
/// would be a way to lose output rather than a way to keep it whole. `Console`
/// is an `immediate` row, so this runs where the last handle did — a bounded
/// flush of at most `MAX_CONSOLE_LINE`, taking the backend once.
impl Drop for ConsoleObject {
    fn drop(&mut self) {
        self.line.lock().finish();
    }
}
