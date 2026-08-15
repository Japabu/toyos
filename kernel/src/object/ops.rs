//! What a syscall does to the object a handle names.
//!
//! Every function here dispatches on [`KObjectRef`] with no `_` arm, so a new
//! object type is a compile error at each of them rather than a silent
//! `PermissionDenied`. Authorization is *not* here: the caller has already
//! resolved the handle with the rights the call needs, and this module never
//! sees a handle number.

use alloc::string::String;
use alloc::vec::Vec;

use toyos_abi::handle::{RawHandle, Rights};
use toyos_abi::syscall::{FileType, OpenFlags, SeekFrom, SyscallError};

use crate::drivers::serial;
use crate::file_cache;
use crate::io_uring::Source;
use crate::pipe::{self, PipeId};
use crate::process::PipeMap;
use crate::user_ptr::{UserBytes, UserBytesMut};
use crate::{device as device_registry, keyboard, mouse};

use super::device::DeviceClaim;
use super::file::{FileObject, OpenFileState};
use super::handle::{HandleEntry, HandleTable};
use super::KObjectRef;

/// What a freshly created object's one handle carries.
///
/// One function rather than a right chosen at each construction site, so
/// "which rights does a pipe read end have?" has one answer and a new call site
/// cannot invent a wider one. Narrowing is `SYS_HANDLE_DUP`'s job and happens
/// after this.
pub fn initial_rights(object: &KObjectRef) -> Rights {
    const BASE: Rights = Rights::DUP.union(Rights::TRANSFER).union(Rights::WAIT);
    match object {
        // `MAP` is `SYS_PIPE_MAP`: the ring page is the pipe's, and either end
        // may window it.
        KObjectRef::PipeRead(_) => BASE.union(Rights::READ).union(Rights::MAP),
        KObjectRef::PipeWrite(_) => BASE.union(Rights::WRITE).union(Rights::MAP),
        KObjectRef::Connection(_) => {
            BASE.union(Rights::READ).union(Rights::WRITE).union(Rights::MAP)
        }
        KObjectRef::File(_) => BASE.union(Rights::READ).union(Rights::WRITE),
        // **No `DUP`.** A claim admits exactly one handle, which is what makes
        // exclusivity a property of the type rather than of a check in `dup`.
        KObjectRef::Device(_) => {
            Rights::TRANSFER.union(Rights::WAIT).union(Rights::READ).union(Rights::WRITE)
        }
        KObjectRef::Console(_) => BASE.union(Rights::READ).union(Rights::WRITE),
        KObjectRef::Acceptor(_) => BASE.union(Rights::READ),
        KObjectRef::IoUring(_) => {
            BASE.union(Rights::READ).union(Rights::WRITE).union(Rights::MAP)
        }
        // Every bit on a `SysCap` is an authority init decides per program, so
        // there is no sensible default and the creator states it.
        KObjectRef::SysCap(_) => Rights::NONE,
        // `MAP` is the whole of it. A region is a thing to look at, and every
        // question about what is in it is asked of the memory rather than of
        // the handle.
        KObjectRef::SharedMem(_) => {
            Rights::DUP.union(Rights::TRANSFER).union(Rights::MAP)
        }
        // A connector is a ticket to a service and has no read or write path
        // at all: the only things to do with one are put it in a namespace and
        // give that namespace away.
        KObjectRef::Connector(_) => Rights::DUP.union(Rights::TRANSFER),
        // `READ` is what resolving a name through it takes, and what narrowing
        // one into a child's takes.
        KObjectRef::Namespace(_) => Rights::DUP.union(Rights::TRANSFER).union(Rights::READ),
        // `WAIT` takes the exit code, `MANAGE` kills, `READ` samples the
        // accounting. A spawner gets all three, and narrows on the way to
        // whoever it hands the process on to.
        KObjectRef::Process(_) => BASE.union(Rights::READ).union(Rights::MANAGE),
    }
}

/// Install a new object at the next free slot, with the rights its type gets.
pub fn install(table: &mut HandleTable, object: KObjectRef) -> Result<RawHandle, SyscallError> {
    let rights = initial_rights(&object);
    table
        .install(HandleEntry::new(object, rights))
        .map_err(|_| SyscallError::ResourceExhausted)
}

/// A file opened at `path`, installed in `table`.
///
/// Takes the VFS lock itself and gives it up before the object exists, so a
/// refused install drops the `OpenFileState` — and re-takes the lock in its
/// `Drop` — without the *VFS* lock held. **Not with nothing held**, which this
/// used to claim: its one caller runs it inside `with_fd_owner_data`, so the
/// process's own lock is still there. What the sequencing buys is that the VFS
/// lock is not taken twice, and that is all it buys.
pub fn open(table: &mut HandleTable, path: &str, flags: OpenFlags) -> u64 {
    let writable = flags.contains(OpenFlags::WRITE);
    let create = flags.contains(OpenFlags::CREATE);
    let truncate = flags.contains(OpenFlags::TRUNCATE);
    let append = flags.contains(OpenFlags::APPEND);

    let opened = {
        let mut vfs = crate::vfs::lock();

        if create {
            let (_, file) = vfs.resolve_path("/", path);
            if file.is_empty() {
                return SyscallError::InvalidArgument.to_u64();
            }
        }

        if truncate && create {
            let mtime = crate::clock::nanos_since_boot();
            // A name that was not there is the ordinary case and not a failure
            // of this open. Anything else is: truncating past it would create a
            // file over one the mount could not tell us about.
            match vfs.delete(path) {
                Ok(()) | Err(SyscallError::NotFound) => {}
                Err(e) => return e.to_u64(),
            }
            vfs.create_file(path, mtime).map(|file_id| (file_id, mtime, 0))
        } else {
            // **`CREATE` acts on `NotFound` and on nothing else.** This used to
            // be the `None` arm of an `Option`, so a mount that would not
            // answer took the same branch as a name that is not there: one
            // refused transfer had a fresh empty file created over a file that
            // exists, and the next write and flush made that permanent.
            match vfs.open_file(path) {
                Ok(file_id) => vfs.file_mtime(path).map(|mtime| {
                    let position =
                        if append { file_cache::size(file_id) as usize } else { 0 };
                    (file_id, mtime, position)
                }),
                Err(SyscallError::NotFound) if create => {
                    let mtime = crate::clock::nanos_since_boot();
                    vfs.create_file(path, mtime).map(|file_id| (file_id, mtime, 0))
                }
                Err(e) => Err(e),
            }
        }
    };

    let (file_id, mtime, position) = match opened {
        Ok(v) => v,
        Err(e) => return e.to_u64(),
    };
    let object = KObjectRef::File(FileObject::new(OpenFileState {
        path: String::from(path),
        file_id,
        position,
        modified: false,
        mtime,
    }));
    // **`writable` is a right, not a field.** A write to a read-only file
    // answers `PermissionDenied` because the handle does not carry `WRITE`,
    // which is the same word the field's check produced and one fewer place
    // for the two to disagree.
    let mut rights = initial_rights(&object);
    if !writable {
        rights = rights.without(Rights::WRITE);
    }
    match table.install(HandleEntry::new(object, rights)) {
        Ok(h) => h.0 as u64,
        Err(_) => SyscallError::ResourceExhausted.to_u64(),
    }
}

/// Release one handle.
///
/// What the object *holds* is given back by its own zero-handle hook. What is
/// left here is the two things that are the *process's* and not the object's,
/// and neither is written per object kind:
///
/// `pipe_maps` is the process's live `SYS_PIPE_MAP` windows. A window's warrant
/// is the handle: past the last one naming a pipe, nothing holds the ring page
/// and the PMM may hand it to anything, so the mapping has to go with the
/// handle rather than with the process. [`close_all`] needs no such argument —
/// its only caller is process teardown, which destroys the address space the
/// windows are in.
pub fn close(
    table: &mut HandleTable,
    h: RawHandle,
    pipe_maps: &mut Vec<PipeMap>,
) -> Result<(), super::HandleError> {
    let entry = table.remove(h)?;
    let object = entry.object().clone();
    // The decrement — and the deferred hook it may enqueue — happen here, with
    // the table's own borrow already given up.
    drop(entry);
    for id in [pipe_id_read(&object), pipe_id_write(&object)].into_iter().flatten() {
        let still_held = table.iter().any(|(_, e)| {
            pipe_id_read(e.object()) == Some(id) || pipe_id_write(e.object()) == Some(id)
        });
        if !still_held {
            if let Some(pt) = crate::scheduler::current_address_space() {
                crate::process::revoke_pipe_maps(pipe_maps, &pt, id);
            }
        }
    }
    let sources = if ends_its_sources(&object) {
        [read_source(&object), write_source(&object)]
    } else {
        [None, None]
    };
    if sources.iter().any(|s| s.is_some()) {
        crate::io_uring::remove_fd(&sources);
    }
    Ok(())
}

/// Does releasing one handle to `object` end the sources it names?
///
/// **`io_uring::remove_fd` cancels by source across every ring in the machine**,
/// which is what a pipe needs — a client closing its end must complete the
/// server's poll on the other, and an fd number means nothing outside the
/// process that owns it. It is also why the question has to be asked: a source
/// that does *not* end when a handle does gets every poll on it cancelled by
/// whoever happened to close a handle naming it, in every other process.
///
/// **Two rows answer `false` and both are named by a handle any number of
/// processes hold.** `SysCap` maps to [`Source::Log`], and the machine's log is
/// not something a capability going away ends: closing one is one process
/// putting down its authority to read a stream that outlives every handle. That
/// was live and latent — `ops::close` on *any* `SysCap` cancelled *every*
/// pending log poll in the machine with `-NotFound`, which `/bin/logd`'s
/// read-then-park loop turns from latent into a daemon that stops reading the
/// moment anything anywhere closes a capability. `Console` maps to
/// [`Source::Keyboard`] for the same reason and has the same shape: every
/// process that has a console has its own object over the one keyboard (§4.4),
/// so one of them closing stdin would cancel the compositor's key poll.
///
/// Every other row answers `true` because its source really is the object's:
/// a pipe end, a connection, a port and a device claim each go away with their
/// last handle, and a claim admits exactly one handle by construction, so
/// "every ring watching it" is the one holder's.
fn ends_its_sources(object: &KObjectRef) -> bool {
    match object {
        KObjectRef::PipeRead(_)
        | KObjectRef::PipeWrite(_)
        | KObjectRef::Connection(_)
        | KObjectRef::Acceptor(_)
        | KObjectRef::Device(_) => true,
        KObjectRef::Console(_) | KObjectRef::SysCap(_) => {
            // The negative control: the behaviour above, restored, so
            // `log_poll_outlives_a_close` reds on the tree that had it.
            crate::actuator::log_close_cancels_any_syscap()
        }
        // No source at all, so the answer decides nothing.
        KObjectRef::File(_)
        | KObjectRef::IoUring(_)
        | KObjectRef::Connector(_)
        | KObjectRef::Namespace(_)
        | KObjectRef::SharedMem(_)
        | KObjectRef::Process(_) => false,
    }
}

/// Release every handle a process holds. Called by exit *and by kill*, so the
/// drops below are on the path a process taken down by another CPU follows —
/// this kernel does not unwind, and a `Drop` that only ran on the orderly path
/// would guarantee nothing.
pub fn close_all(table: &mut HandleTable) {
    for entry in table.drain() {
        drop(entry);
    }
}

pub fn pipe_id_read(object: &KObjectRef) -> Option<PipeId> {
    match object {
        KObjectRef::PipeRead(r) => Some(r.id()),
        KObjectRef::Connection(c) => Some(c.rx()),
        KObjectRef::PipeWrite(_) | KObjectRef::File(_) | KObjectRef::Device(_)
        | KObjectRef::Console(_) | KObjectRef::Acceptor(_) | KObjectRef::IoUring(_)
        | KObjectRef::SysCap(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::SharedMem(_) | KObjectRef::Process(_) => None,
    }
}

pub fn pipe_id_write(object: &KObjectRef) -> Option<PipeId> {
    match object {
        KObjectRef::PipeWrite(w) => Some(w.id()),
        KObjectRef::Connection(c) => Some(c.tx()),
        KObjectRef::PipeRead(_) | KObjectRef::File(_) | KObjectRef::Device(_)
        | KObjectRef::Console(_) | KObjectRef::Acceptor(_) | KObjectRef::IoUring(_)
        | KObjectRef::SysCap(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::SharedMem(_) | KObjectRef::Process(_) => None,
    }
}

pub fn read_source(object: &KObjectRef) -> Option<Source> {
    match object {
        KObjectRef::PipeRead(r) => Some(Source::PipeReadable(r.id())),
        KObjectRef::Connection(c) => Some(Source::PipeReadable(c.rx())),
        KObjectRef::Acceptor(a) => Some(Source::Port(a.port())),
        KObjectRef::Console(_) => Some(Source::Keyboard),
        KObjectRef::Device(d) => match d.class() {
            device_registry::DeviceType::Keyboard => Some(Source::Keyboard),
            device_registry::DeviceType::Mouse => Some(Source::Mouse),
            device_registry::DeviceType::Nic => Some(Source::Network),
            device_registry::DeviceType::HdaAudio => Some(Source::Hda),
            device_registry::DeviceType::VirtioSound => Some(Source::VirtioSound),
            device_registry::DeviceType::Framebuffer => None,
        },
        // **The `SysCap` is what a log reader parks on, and the rights on the
        // handle are what decide whether either half means anything.** This
        // function is handed the object and never the handle, so the source is
        // named unconditionally; `WAIT` is what the poll path already demands
        // before it gets here, and `Rights::LOG` is what `SYS_LOG_READ` demands
        // to answer with a record. A cap holding one without the other can
        // therefore park on a stream it may not read, or read a stream it may
        // not park on — and `toyos_manifest`'s `logread` grants both, because
        // the one program whose whole loop is read-then-park would be trapped
        // by a name that granted only the first.
        KObjectRef::SysCap(_) => Some(Source::Log),
        KObjectRef::PipeWrite(_) | KObjectRef::File(_) | KObjectRef::IoUring(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::SharedMem(_) | KObjectRef::Process(_) => None,
    }
}

pub fn write_source(object: &KObjectRef) -> Option<Source> {
    match object {
        KObjectRef::PipeWrite(w) => Some(Source::PipeWritable(w.id())),
        KObjectRef::Connection(c) => Some(Source::PipeWritable(c.tx())),
        KObjectRef::PipeRead(_) | KObjectRef::File(_) | KObjectRef::Device(_)
        | KObjectRef::Console(_) | KObjectRef::Acceptor(_) | KObjectRef::IoUring(_)
        | KObjectRef::SysCap(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::SharedMem(_) | KObjectRef::Process(_) => None,
    }
}

fn read_file(file: &FileObject, buf: &mut UserBytesMut) -> Option<u64> {
    file.with(|state| {
        let size = file_cache::size(state.file_id) as usize;
        let available = size.saturating_sub(state.position);
        let count = buf.len().min(available);
        if count == 0 {
            return Some(0);
        }
        let mut read = 0;
        let mut refused = false;
        while read < count {
            let abs_pos = state.position + read;
            let page_idx = (abs_pos / 4096) as u32;
            let offset_in_page = abs_pos % 4096;
            let remaining_in_page = 4096 - offset_in_page;
            let to_read = remaining_in_page.min(count - read);
            // A page the device would not give back is not a page of zeros.
            // This stops short of it rather than handing the caller a hole
            // under a success; short counts are what `read` means.
            if file_cache::read_page(
                state.file_id,
                page_idx,
                offset_in_page,
                &mut buf.sub(read, to_read),
            )
            .is_err()
            {
                refused = true;
                break;
            }
            read += to_read;
        }
        if read == 0 && refused {
            return Some(SyscallError::Io.to_u64());
        }
        state.position += read;
        Some(read as u64)
    })
}

/// Read a device claim.
///
/// **It takes the table because the description installs handles.** A
/// description is a set of buffers, and the process being told about them is
/// the one that must be able to map them — which is never the process that
/// minted the claim, because init mints every claim and holds none of them.
/// Every other read runs under the borrow `get_ref` hands out, which is what
/// keeps the two hottest syscalls in the kernel free of an atomic refcount.
pub fn read_device(
    claim: &DeviceClaim,
    table: &mut HandleTable,
    buf: &mut UserBytesMut,
) -> Option<u64> {
    match claim.class() {
        // **A read of an input device reads the queue and drives no
        // hardware.** Both of these polled xHCI first, which made whichever
        // thread happened to read the mouse into the driver's enumeration and
        // recovery engine — on the T14 that was the compositor's own mouse
        // read, and the desktop froze for multi-second stretches with a live
        // kernel and nothing dropped. `drain_irqs` calls the same function at
        // the top of every scheduler pass, so a reader gives up at most one
        // pass of latency.
        device_registry::DeviceType::Keyboard | device_registry::DeviceType::Mouse => {
            match claim.class() {
            device_registry::DeviceType::Keyboard => {
                let event_size = core::mem::size_of::<keyboard::RawKeyEvent>();
                let mut count = 0;
                while count + event_size <= buf.len() {
                    let Some(event) = keyboard::try_read_event() else { break };
                    buf.write_at(count, event.as_bytes());
                    count += event_size;
                }
                if count > 0 { Some(count as u64) } else { None }
            }
            device_registry::DeviceType::Mouse => {
                let event_size = core::mem::size_of::<mouse::MouseEvent>();
                let mut count = 0;
                while count + event_size <= buf.len() {
                    let Some(event) = mouse::try_read_event() else { break };
                    buf.write_at(count, event.as_bytes());
                    count += event_size;
                }
                if count > 0 { Some(count as u64) } else { None }
            }
            other => panic!("a {other:?} claim answers with events"),
            }
        }
        device_registry::DeviceType::Framebuffer | device_registry::DeviceType::Nic => {
            Some(claim.describe(table, buf))
        }
        device_registry::DeviceType::HdaAudio => {
            if !claim.info_read() {
                return Some(claim.describe(table, buf));
            }
            if buf.len() < toyos_abi::audio::AudioCompletionRecord::SIZE {
                return Some(SyscallError::InvalidArgument.to_u64());
            }
            let n = crate::drivers::hda::drain_completed(buf);
            if n == 0 { None } else { Some(n as u64) }
        }
        device_registry::DeviceType::VirtioSound => {
            if !claim.info_read() {
                return Some(claim.describe(table, buf));
            }
            if buf.len() < toyos_abi::audio::AudioCompletionRecord::SIZE {
                return Some(SyscallError::InvalidArgument.to_u64());
            }
            // Completion records, oldest first. Empty → None: blocking reads
            // park on `waitqs::AUDIO`, nonblocking reads get WouldBlock.
            let n = crate::drivers::virtio_sound::drain_completed(buf);
            if n == 0 { None } else { Some(n as u64) }
        }
    }
}

/// Read whatever a handle names, except a device claim.
///
/// [`read_device`] is separate because it needs the table mutably and this runs
/// under the borrow the handle was resolved through. Its arm here is
/// unreachable and says so rather than silently answering `PermissionDenied`.
pub fn try_read(object: &KObjectRef, buf: &mut UserBytesMut) -> Option<u64> {
    match object {
        KObjectRef::File(f) => read_file(f, buf),
        KObjectRef::PipeRead(r) => pipe::try_read(r.id(), buf).map(|n| n as u64),
        KObjectRef::Connection(c) => pipe::try_read(c.rx(), buf).map(|n| n as u64),
        KObjectRef::Device(_) => unreachable!("a device claim is read by `read_device`"),
        KObjectRef::Console(_) => {
            let mut count = 0usize;
            while count < buf.len() {
                if let Some(b) = serial::try_read_byte() {
                    buf.write_at(count, &[b]);
                    count += 1;
                    if b == b'\n' || b == b'\r' {
                        break;
                    }
                } else if count > 0 {
                    break;
                } else {
                    return None;
                }
            }
            Some(count as u64)
        }
        KObjectRef::PipeWrite(_) | KObjectRef::Acceptor(_) | KObjectRef::IoUring(_)
        | KObjectRef::SysCap(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::SharedMem(_) | KObjectRef::Process(_) => Some(SyscallError::PermissionDenied.to_u64()),
    }
}

fn write_pipe(id: PipeId, buf: &UserBytes) -> Option<u64> {
    match pipe::try_write(id, buf) {
        Some(pipe::PipeWrite::BrokenPipe) => Some(SyscallError::NotFound.to_u64()),
        Some(pipe::PipeWrite::NoMemory) => Some(SyscallError::ResourceExhausted.to_u64()),
        Some(pipe::PipeWrite::Wrote(n)) => Some(n as u64),
        None => None,
    }
}

pub fn try_write(object: &KObjectRef, buf: &UserBytes) -> Option<u64> {
    match object {
        KObjectRef::File(f) => f.with(|state| {
            let mut written = 0;
            let mut refused = false;
            while written < buf.len() {
                let abs_pos = state.position + written;
                let page_idx = (abs_pos / 4096) as u32;
                let offset_in_page = abs_pos % 4096;
                let remaining_in_page = 4096 - offset_in_page;
                let to_write = remaining_in_page.min(buf.len() - written);
                // A partial write whose page could not be re-read off the
                // device is refused rather than merged into zeros, so this
                // stops short instead of claiming bytes that are not in the
                // file.
                if file_cache::write_page(
                    state.file_id,
                    page_idx,
                    offset_in_page,
                    &buf.sub(written, to_write),
                )
                .is_err()
                {
                    refused = true;
                    break;
                }
                written += to_write;
            }
            if written == 0 && refused {
                return Some(SyscallError::Io.to_u64());
            }
            state.position += written;
            state.modified = true;
            state.mtime = crate::clock::nanos_since_boot();
            Some(written as u64)
        }),
        KObjectRef::PipeWrite(w) => write_pipe(w.id(), buf),
        KObjectRef::Connection(c) => write_pipe(c.tx(), buf),
        KObjectRef::Console(c) => {
            // Into this holder's line buffer, which emits whole lines under one
            // `BackendGuard` (§4.4). It used to be a lossless append to the byte
            // ring that something else drained later, then a direct bounded
            // write to the backend; both made the unit of interleaving a `write`
            // syscall, and `println!` issues two of those per line. The
            // `console-unbuffered` actuator restores the second of those states.
            //
            // **The whole write is always accepted.** The buffer is the kernel's
            // and a short count would make a caller re-send bytes it already
            // handed over.
            c.write(buf);
            Some(buf.len() as u64)
        }
        KObjectRef::PipeRead(_) | KObjectRef::Device(_) | KObjectRef::Acceptor(_)
        | KObjectRef::IoUring(_) | KObjectRef::SharedMem(_) | KObjectRef::SysCap(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::Process(_) => {
            Some(SyscallError::PermissionDenied.to_u64())
        }
    }
}

pub fn seek(object: &KObjectRef, pos: SeekFrom) -> u64 {
    let KObjectRef::File(file) = object else {
        return SyscallError::PermissionDenied.to_u64();
    };
    file.with(|state| {
        let size = file_cache::size(state.file_id) as usize;
        let new_pos = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::Current(n) => (state.position as i64).checked_add(n).unwrap_or(-1),
            SeekFrom::End(n) => (size as i64).checked_add(n).unwrap_or(-1),
        };
        if new_pos < 0 {
            return SyscallError::InvalidArgument.to_u64();
        }
        state.position = (new_pos as usize).min(size);
        state.position as u64
    })
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Stat {
    pub file_type: u64,
    pub size: u64,
    pub mtime: u64,
}

/// What kind of thing this is, and how big.
///
/// Every object answers, so this returns a value rather than an `Option`: the
/// one way to have no answer is a handle that does not resolve, which the
/// caller has already ruled out.
pub fn fstat(object: &KObjectRef) -> Stat {
    let plain = |t: FileType| Stat { file_type: t as u64, size: 0, mtime: 0 };
    match object {
        KObjectRef::File(f) => f.with(|state| Stat {
            file_type: FileType::File as u64,
            size: file_cache::size(state.file_id),
            mtime: state.mtime,
        }),
        KObjectRef::PipeRead(r) => {
            plain(if r.is_tty() { FileType::Tty } else { FileType::Pipe })
        }
        KObjectRef::PipeWrite(w) => {
            plain(if w.is_tty() { FileType::Tty } else { FileType::Pipe })
        }
        KObjectRef::Connection(_) => plain(FileType::Socket),
        KObjectRef::Console(_) => plain(FileType::Serial),
        KObjectRef::Acceptor(_) => plain(FileType::Pipe),
        KObjectRef::SharedMem(m) => Stat {
            file_type: FileType::Unknown as u64,
            size: m.size(),
            mtime: 0,
        },
        KObjectRef::IoUring(_) | KObjectRef::SysCap(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::Process(_) => plain(FileType::Unknown),
        KObjectRef::Device(d) => plain(match d.class() {
            device_registry::DeviceType::Keyboard => FileType::Keyboard,
            device_registry::DeviceType::Mouse => FileType::Mouse,
            device_registry::DeviceType::Framebuffer => FileType::Framebuffer,
            device_registry::DeviceType::Nic => FileType::Nic,
            device_registry::DeviceType::HdaAudio
            | device_registry::DeviceType::VirtioSound => FileType::Unknown,
        }),
    }
}

/// `SYS_FSYNC`: the file's bytes on the device, **and the device told to commit
/// them**.
///
/// **The second step is L6's, and it is a change to a shipped syscall's
/// semantics rather than an implementation detail** (§12.4). This used to be
/// `flush_file` alone, which puts the data, the FAT and the directory entry on
/// the volume and stops there — the stick's own write cache still holds them,
/// so a power cut after a successful `fsync` could lose what it returned `Ok`
/// for. The only caller in the machine that did the second step was
/// `log_file.rs`, in the kernel, from the idle loop.
///
/// It is not optional now: `/bin/logd` publishes `LOG_DURABLE_NS` off this
/// call's result, and a panicking kernel stops waiting for its own report when
/// that word passes the report's timestamp. An `fsync` that stopped at the page
/// cache would make the whole durability contract a claim about nothing. The
/// alternative considered and rejected was a second syscall for logd alone,
/// which needs a number, needs discussion, and would make every *other*
/// `fsync` in the machine quietly weaker than the one program that noticed.
/// `log_is_durable_after_fsync` is the gate, and it reds on the old behaviour.
pub fn fsync(object: &KObjectRef) -> u64 {
    let KObjectRef::File(file) = object else {
        return SyscallError::PermissionDenied.to_u64();
    };
    let flush = file.with(|state| {
        state
            .modified
            .then(|| (state.path.clone(), state.file_id, state.mtime))
    });
    let Some((path, file_id, mtime)) = flush else { return 0 };
    // Outside `FileObject`'s own lock: the VFS lock is taken here and in
    // `OpenFileState::drop`, and holding both in one order here and the other
    // there is the deadlock this ordering exists to avoid.
    let mut vfs = crate::vfs::lock();
    if let Err(e) = vfs.flush_file(&path, file_id, mtime) {
        return e.to_u64();
    }
    // Under the same acquisition as the write-back above, deliberately: two
    // acquisitions would let another writer's data reach the volume between
    // them and be committed by this caller's flush, which is harmless, and
    // would let this caller's own file be *unmounted* between them, which is
    // not.
    if let Err(e) = vfs.sync_for_path(&path) {
        return e.to_u64();
    }
    drop(vfs);
    file.with(|state| state.modified = false);
    0
}

pub fn ftruncate(object: &KObjectRef, size: u64) -> u64 {
    let KObjectRef::File(file) = object else {
        return SyscallError::PermissionDenied.to_u64();
    };
    file.with(|state| {
        file_cache::set_size(state.file_id, size);
        if state.position > size as usize {
            state.position = size as usize;
        }
        state.modified = true;
        state.mtime = crate::clock::nanos_since_boot();
        0
    })
}

pub fn has_data(object: &KObjectRef) -> bool {
    match object {
        KObjectRef::PipeRead(r) => pipe::has_data(r.id()),
        KObjectRef::Connection(c) => pipe::has_data(c.rx()),
        KObjectRef::Console(_) => serial::has_data(),
        KObjectRef::Acceptor(a) => a.has_pending(),
        KObjectRef::File(_) => true,
        KObjectRef::Device(d) => match d.class() {
            device_registry::DeviceType::Keyboard => keyboard::has_data(),
            device_registry::DeviceType::Mouse => mouse::has_data(),
            device_registry::DeviceType::Nic => crate::net::has_packet(),
            device_registry::DeviceType::Framebuffer => true,
            device_registry::DeviceType::HdaAudio => {
                !d.info_read() || crate::drivers::hda::has_pending()
            }
            device_registry::DeviceType::VirtioSound => {
                !d.info_read() || crate::drivers::virtio_sound::has_pending()
            }
        },
        KObjectRef::PipeWrite(_) | KObjectRef::IoUring(_) | KObjectRef::SysCap(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::SharedMem(_) | KObjectRef::Process(_) => false,
    }
}

pub fn has_space(object: &KObjectRef) -> bool {
    match object {
        KObjectRef::PipeWrite(w) => pipe::has_space(w.id()),
        KObjectRef::Connection(c) => pipe::has_space(c.tx()),
        KObjectRef::File(_) | KObjectRef::Console(_) => true,
        KObjectRef::PipeRead(_) | KObjectRef::Device(_) | KObjectRef::Acceptor(_)
        | KObjectRef::IoUring(_) | KObjectRef::SysCap(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::SharedMem(_) | KObjectRef::Process(_) => false,
    }
}

/// Mark one end of a pipe as a terminal.
///
/// **Per end, not per pipe.** Its one caller marks both ends of a pair
/// separately, so a flag on the shared ring would be a wider claim than
/// anything ever makes — and `FileType::Tty` is then read off the end that was
/// marked rather than off a variant the mark had to swap the handle into.
pub fn mark_tty(object: &KObjectRef) -> u64 {
    match object {
        KObjectRef::PipeRead(r) => {
            r.mark_tty();
            0
        }
        KObjectRef::PipeWrite(w) => {
            w.mark_tty();
            0
        }
        KObjectRef::Connection(_) | KObjectRef::File(_) | KObjectRef::Device(_)
        | KObjectRef::Console(_) | KObjectRef::Acceptor(_) | KObjectRef::IoUring(_)
        | KObjectRef::SysCap(_) | KObjectRef::SharedMem(_)
        | KObjectRef::Connector(_) | KObjectRef::Namespace(_)
        | KObjectRef::Process(_) => SyscallError::InvalidArgument.to_u64(),
    }
}
