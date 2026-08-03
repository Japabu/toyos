//! Framed IPC over sockets (bidirectional pipe pairs).
//!
//! Wire format: `[u32 msg_type][u32 len][len bytes payload]`.

use toyos_abi::Fd;
use toyos_abi::syscall::{self, SyscallError};
use crate::{AsHandle, Handle};

/// A type that may cross an IPC boundary as a payload.
///
/// # Safety
/// Implementors must be `#[repr(C)]`, contain no padding bytes, hold no
/// pointers, and have no invalid bit patterns. All three are load-bearing:
/// [`send`] publishes `size_of::<T>()` bytes of the sender's memory to a peer,
/// so a padding byte no field owns is whatever the sender's stack held; and
/// [`recv_payload`] reinterprets bytes a *peer* chose, so a `bool`, a
/// fieldless enum or a `NonZeroU32` would be UB the moment it arrives.
///
/// Declare payload structs with [`ipc_payload!`], which proves the first two
/// mechanically. A hand-written `unsafe impl` is checking them by eye.
///
/// `usize` and `isize` are deliberately absent: a wire type has one width.
pub unsafe trait IpcPayload: Copy {
    /// Exists so [`ipc_payload!`]'s padding assertion can only sum field
    /// types that are payloads themselves — which is what makes the
    /// no-padding property hold through a nested struct.
    #[doc(hidden)]
    const SIZE: usize = core::mem::size_of::<Self>();
}

macro_rules! payload_primitives {
    ($($t:ty),* $(,)?) => { $(unsafe impl IpcPayload for $t {})* };
}
payload_primitives!(u8, u16, u32, u64, i8, i16, i32, i64, f32, f64);

unsafe impl<T: IpcPayload, const N: usize> IpcPayload for [T; N] {}

/// Declare a `#[repr(C)]` IPC payload struct.
///
/// The expansion asserts that the struct's size equals the sum of its field
/// sizes, which for `repr(C)` holds exactly when there is no padding, and it
/// sums them through [`IpcPayload::SIZE`], so every field must be a payload
/// too. A struct that would publish bytes no field owns does not compile, and
/// neither does one carrying a type whose bit patterns are not all valid.
#[macro_export]
macro_rules! ipc_payload {
    ($(
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $($(#[$fmeta:meta])* $fvis:vis $field:ident : $ty:ty),* $(,)?
        }
    )+) => {$(
        $(#[$meta])*
        #[repr(C)]
        #[derive(Clone, Copy)]
        $vis struct $name {
            $($(#[$fmeta])* $fvis $field: $ty,)*
        }

        const _: () = assert!(
            core::mem::size_of::<$name>()
                == 0 $(+ <$ty as $crate::ipc::IpcPayload>::SIZE)*,
            concat!(
                stringify!($name),
                " has padding: an IPC payload must not publish bytes no field owns",
            ),
        );

        unsafe impl $crate::ipc::IpcPayload for $name {}
    )+};
}

/// The largest payload this endpoint will frame, in either direction.
///
/// Derived, not picked: the largest payload struct in the tree is
/// `window::CreateWindowRequest` at 40 bytes, and the largest byte payload any
/// protocol here produces is 4096 — the threshold at which `window`'s
/// clipboard set and the compositor's paste both switch to shared memory
/// instead. 8192 is one doubling of headroom, so a protocol can grow its
/// largest inline message without the SDK becoming the thing that refuses it.
///
/// What the bound buys is a refusal in O(1). A peer's `len` sizes a
/// [`recv_bytes`] tail-skip, so `len = u32::MAX` obliges the reader to consume
/// every byte that peer will ever send before it returns to its event loop.
/// Bounded, that frame is an error at the header and the peer can be dropped.
pub const MAX_FRAME_LEN: u32 = 8192;

/// A frame header. `len` is private because it carries an invariant a peer
/// does not: every `IpcHeader` in existence has a length within
/// [`MAX_FRAME_LEN`], because [`IpcHeader::from_wire`] is the only way to make
/// one out of bytes somebody else chose.
#[derive(Clone, Copy)]
pub struct IpcHeader {
    pub msg_type: u32,
    len: u32,
}

impl IpcHeader {
    const WIRE_SIZE: usize = 8;

    /// Build a header from a length that has not been trusted yet.
    pub fn from_wire(msg_type: u32, len: u32) -> Result<Self, IpcError> {
        if len > MAX_FRAME_LEN {
            return Err(IpcError::Malformed);
        }
        Ok(Self { msg_type, len })
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    fn to_wire(self) -> [u8; Self::WIRE_SIZE] {
        let mut bytes = [0u8; Self::WIRE_SIZE];
        bytes[..4].copy_from_slice(&self.msg_type.to_ne_bytes());
        bytes[4..].copy_from_slice(&self.len.to_ne_bytes());
        bytes
    }

    /// The same header, built from a length this endpoint chose itself.
    fn frame(msg_type: u32, len: usize) -> Result<Self, IpcError> {
        if len > MAX_FRAME_LEN as usize {
            return Err(IpcError::TooLarge);
        }
        Ok(Self { msg_type, len: len as u32 })
    }
}

/// The largest typed payload [`try_send`] frames without allocating.
///
/// Every `ipc_payload!` struct in the tree is far below this — the largest is
/// `window::CreateWindowRequest` at 40 bytes. A type past it does not compile,
/// because the alternative would be a payload split across two writes, and a
/// non-blocking send that can be half-refused is the thing [`try_send`] exists
/// to make unrepresentable.
pub const MAX_TYPED_PAYLOAD: usize = 64;

#[derive(Debug)]
pub enum IpcError {
    Disconnected,
    /// The peer's frame does not describe a message this endpoint can read:
    /// a length past [`MAX_FRAME_LEN`], or one shorter than the payload the
    /// caller asked for.
    Malformed,
    /// This endpoint will not frame a payload that large. Distinct from
    /// [`IpcError::Malformed`] because the two blame different sides.
    TooLarge,
    Syscall(SyscallError),
}

/// Why a whole frame could not be handed over without blocking.
#[derive(Debug)]
pub enum TrySendError {
    /// The peer's pipe would not take the whole frame in one write.
    ///
    /// **The connection is no longer at a message boundary.** The kernel
    /// accepts a short write and there is no way to retract the part it took,
    /// so there is nothing here to retry: the only correct answer is to drop
    /// the peer. That is not a harsh reading of a busy moment — a peer that
    /// cannot take one frame has an entire pipe of unread messages behind it.
    Full,
    /// This endpoint will not frame a payload that large.
    TooLarge,
    Syscall(SyscallError),
}

/// An IPC connection. Created by [`crate::services::accept`] or
/// [`crate::services::connect`].
pub struct Connection(pub(crate) Handle);

impl AsHandle for Connection {
    fn as_handle(&self) -> Fd { self.0.fd() }
}

impl Connection {
    pub fn fd(&self) -> Fd { self.0.fd() }

    pub fn send<T: IpcPayload>(&self, msg_type: u32, payload: &T) -> Result<(), IpcError> {
        send(self.fd(), msg_type, payload)
    }

    pub fn signal(&self, msg_type: u32) -> Result<(), IpcError> {
        signal(self.fd(), msg_type)
    }

    pub fn send_bytes(&self, msg_type: u32, data: &[u8]) -> Result<(), IpcError> {
        send_bytes(self.fd(), msg_type, data)
    }

    pub fn try_send<T: IpcPayload>(&self, msg_type: u32, payload: &T) -> Result<(), TrySendError> {
        try_send(self.fd(), msg_type, payload)
    }

    pub fn try_signal(&self, msg_type: u32) -> Result<(), TrySendError> {
        try_signal(self.fd(), msg_type)
    }

    pub fn try_send_bytes(&self, msg_type: u32, data: &[u8]) -> Result<(), TrySendError> {
        try_send_bytes(self.fd(), msg_type, data)
    }

    pub fn recv_header(&self) -> Result<IpcHeader, IpcError> {
        recv_header(self.fd())
    }

    pub fn recv_payload<T: IpcPayload>(&self, header: &IpcHeader) -> Result<T, IpcError> {
        recv_payload(self.fd(), header)
    }

    pub fn recv<T: IpcPayload>(&self) -> Result<(u32, T), IpcError> {
        recv(self.fd())
    }

    pub fn recv_bytes(&self, header: &IpcHeader, buf: &mut [u8]) -> Result<usize, IpcError> {
        recv_bytes(self.fd(), header, buf)
    }

    pub fn read_nonblock(&self, buf: &mut [u8]) -> Result<usize, SyscallError> {
        self.0.read_nonblock(buf)
    }

    pub fn write_nonblock(&self, buf: &[u8]) -> Result<usize, SyscallError> {
        self.0.write_nonblock(buf)
    }
}

// Free functions — used by consumers that hold raw Fds (compositor, netd).
// Will become pub(crate) once all callers migrate to Connection methods.

pub fn send<T: IpcPayload>(fd: Fd, msg_type: u32, payload: &T) -> Result<(), IpcError> {
    let header = IpcHeader::frame(msg_type, core::mem::size_of::<T>())?;
    write_all(fd, &header.to_wire())?;
    write_all(fd, as_bytes(payload))
}

pub fn signal(fd: Fd, msg_type: u32) -> Result<(), IpcError> {
    write_all(fd, &IpcHeader { msg_type, len: 0 }.to_wire())
}

pub fn send_bytes(fd: Fd, msg_type: u32, data: &[u8]) -> Result<(), IpcError> {
    let header = IpcHeader::frame(msg_type, data.len())?;
    write_all(fd, &header.to_wire())?;
    if !data.is_empty() {
        write_all(fd, data)?;
    }
    Ok(())
}

/// Send a framed payload without ever blocking, or refuse the whole frame.
///
/// A server writing to a client it does not trust to read cannot use [`send`]:
/// `write_all` parks in the kernel until the peer drains, which is a client
/// deciding when the server runs again. This is the same frame in one
/// `write_nonblock`, so the peer's backlog is an answer rather than a wait.
pub fn try_send<T: IpcPayload>(fd: Fd, msg_type: u32, payload: &T) -> Result<(), TrySendError> {
    const { assert!(core::mem::size_of::<T>() <= MAX_TYPED_PAYLOAD) };
    let size = core::mem::size_of::<T>();
    let mut frame = [0u8; IpcHeader::WIRE_SIZE + MAX_TYPED_PAYLOAD];
    let header = IpcHeader::frame(msg_type, size).map_err(|_| TrySendError::TooLarge)?;
    frame[..IpcHeader::WIRE_SIZE].copy_from_slice(&header.to_wire());
    frame[IpcHeader::WIRE_SIZE..IpcHeader::WIRE_SIZE + size].copy_from_slice(as_bytes(payload));
    write_whole(fd, &frame[..IpcHeader::WIRE_SIZE + size])
}

/// [`signal`] without blocking. A bare header always fits in one write.
pub fn try_signal(fd: Fd, msg_type: u32) -> Result<(), TrySendError> {
    write_whole(fd, &IpcHeader { msg_type, len: 0 }.to_wire())
}

/// [`send_bytes`] without blocking.
///
/// The frame buffer is [`MAX_FRAME_LEN`] on the stack, so this is for the
/// occasional large message — a clipboard paste — not for a per-frame path.
pub fn try_send_bytes(fd: Fd, msg_type: u32, data: &[u8]) -> Result<(), TrySendError> {
    let mut frame = [0u8; IpcHeader::WIRE_SIZE + MAX_FRAME_LEN as usize];
    let header = IpcHeader::frame(msg_type, data.len()).map_err(|_| TrySendError::TooLarge)?;
    frame[..IpcHeader::WIRE_SIZE].copy_from_slice(&header.to_wire());
    frame[IpcHeader::WIRE_SIZE..IpcHeader::WIRE_SIZE + data.len()].copy_from_slice(data);
    write_whole(fd, &frame[..IpcHeader::WIRE_SIZE + data.len()])
}

pub fn recv_header(fd: Fd) -> Result<IpcHeader, IpcError> {
    let mut bytes = [0u8; IpcHeader::WIRE_SIZE];
    read_exact(fd, &mut bytes)?;
    IpcHeader::from_wire(
        u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    )
}

pub fn recv_payload<T: IpcPayload>(fd: Fd, header: &IpcHeader) -> Result<T, IpcError> {
    let size = core::mem::size_of::<T>();
    if (header.len as usize) < size {
        return Err(IpcError::Malformed);
    }
    let mut val = core::mem::MaybeUninit::<T>::uninit();
    // SAFETY: the slice covers exactly the `T` being filled, and `IpcPayload`
    // promises every bit pattern `read_exact` can write is a valid `T`.
    read_exact(fd, unsafe {
        core::slice::from_raw_parts_mut(val.as_mut_ptr() as *mut u8, size)
    })?;
    skip(fd, header.len as usize - size)?;
    Ok(unsafe { val.assume_init() })
}

/// Receive header + typed payload in one call.
pub fn recv<T: IpcPayload>(fd: Fd) -> Result<(u32, T), IpcError> {
    let header = recv_header(fd)?;
    let payload = recv_payload(fd, &header)?;
    Ok((header.msg_type, payload))
}

/// Receive raw bytes. Returns the number of valid bytes read.
pub fn recv_bytes(fd: Fd, header: &IpcHeader, buf: &mut [u8]) -> Result<usize, IpcError> {
    let count = (header.len as usize).min(buf.len());
    if count > 0 {
        read_exact(fd, &mut buf[..count])?;
    }
    skip(fd, header.len as usize - count)?;
    Ok(count)
}

/// The memory twin of [`recv_payload`], for a reader that already holds the
/// bytes.
///
/// Buffering a whole frame before acting on it is what lets a server stay
/// non-blocking against a peer that stops mid-message, and a buffered payload
/// has no fd left to read it from.
pub fn decode_payload<T: IpcPayload>(bytes: &[u8]) -> Result<T, IpcError> {
    let size = core::mem::size_of::<T>();
    if bytes.len() < size {
        return Err(IpcError::Malformed);
    }
    let mut val = core::mem::MaybeUninit::<T>::uninit();
    // SAFETY: the copy fills exactly the `T` being built from a source at
    // least that long, and `IpcPayload` promises every bit pattern is a valid
    // `T` — the same contract `recv_payload` reads on.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), val.as_mut_ptr() as *mut u8, size);
        Ok(val.assume_init())
    }
}

fn as_bytes<T: IpcPayload>(val: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>()) }
}

fn skip(fd: Fd, mut remaining: usize) -> Result<(), IpcError> {
    let mut buf = [0u8; 128];
    while remaining > 0 {
        let chunk = remaining.min(buf.len());
        read_exact(fd, &mut buf[..chunk])?;
        remaining -= chunk;
    }
    Ok(())
}

fn read_exact(fd: Fd, buf: &mut [u8]) -> Result<(), IpcError> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = syscall::read(fd, &mut buf[offset..]).map_err(IpcError::Syscall)?;
        if n == 0 {
            return Err(IpcError::Disconnected);
        }
        offset += n;
    }
    Ok(())
}

/// All of `buf` in one non-blocking write, or [`TrySendError::Full`].
///
/// A short write is `Full`, not a partial success: the caller asked for a
/// frame, and half a frame is a stream the peer can no longer parse.
fn write_whole(fd: Fd, buf: &[u8]) -> Result<(), TrySendError> {
    match syscall::write_nonblock(fd, buf) {
        Ok(n) if n == buf.len() => Ok(()),
        Ok(_) => Err(TrySendError::Full),
        Err(SyscallError::WouldBlock) => Err(TrySendError::Full),
        Err(e) => Err(TrySendError::Syscall(e)),
    }
}

fn write_all(fd: Fd, buf: &[u8]) -> Result<(), IpcError> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = syscall::write(fd, &buf[offset..]).map_err(IpcError::Syscall)?;
        offset += n;
    }
    Ok(())
}
