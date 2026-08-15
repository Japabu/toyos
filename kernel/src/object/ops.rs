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
    let sources = [read_source(&object), write_source(&object)];
    if sources.iter().any(|s| s.is_some()) {
        crate::io_uring::remove_fd(&sources);
    }
    Ok(())
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
        KObjectRef::PipeWrite(_) | KObjectRef::File(_) | KObjectRef::IoUring(_)
        | KObjectRef::SysCap(_)
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
        KObjectRef::Console(_) => {
            // Straight to the backend in bounded flushes — `write_console`
            // stages into a `MAX_CONSOLE_LINE` buffer and takes the guard per
            // flush — where it used to be a lossless append to the byte ring
            // that something else drained later. **L5 puts `ConsoleObject`'s
            // line buffer in front of this** (§4.4); until then a `println!`
            // still hands the kernel half a line at a time, which is what
            // L5's `console-unbuffered` actuator will stage — so this state is
            // a real prior build rather than an invented one.
            serial::write_console(buf);
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
    if let Err(e) = crate::vfs::lock().flush_file(&path, file_id, mtime) {
        return e.to_u64();
    }
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
