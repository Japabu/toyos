//! Framed IPC over sockets (bidirectional pipe pairs).
//!
//! Wire format: `[u32 msg_type][u32 len][len bytes payload]`.

use toyos_abi::RawHandle;
use toyos_abi::syscall::{self, SyscallError};
use crate::{AsHandle, OwnedHandle};

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
    pub const WIRE_SIZE: usize = 8;

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

/// An IPC connection. A server gets one from [`crate::port::Acceptor::accept`]
/// and a client from [`crate::endow::service`].
pub struct Connection(pub(crate) OwnedHandle);

impl AsHandle for Connection {
    fn as_handle(&self) -> RawHandle { self.0.fd() }
}

impl Connection {
    pub fn fd(&self) -> RawHandle { self.0.fd() }

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

    /// [`send_with_handles`](Self::send_with_handles) for a server that will
    /// not park on a client.
    ///
    /// The handles move first here too, and a refused frame leaves them in the
    /// peer's queue — where the queue releases them when the connection goes,
    /// which is the next thing that happens to a peer this refused.
    pub fn try_send_with_handles<T: IpcPayload>(
        &self,
        handles: &[RawHandle],
        msg_type: u32,
        payload: &T,
    ) -> Result<(), TrySendError> {
        syscall::handle_send(self.fd(), handles).map_err(TrySendError::Syscall)?;
        self.try_send(msg_type, payload)
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

    /// Move `handles` to the peer and then send the frame that announces them.
    ///
    /// **In that order, and this is the only place it is written.** The handles
    /// travel in a queue of their own rather than interleaved with the bytes,
    /// so a peer that has read the frame is guaranteed to find them — and a
    /// peer that has not is guaranteed not to act on them early. Sending the
    /// frame first would make the receiver's `recv_handles` a poll.
    pub fn send_with_handles<T: IpcPayload>(
        &self,
        handles: &[RawHandle],
        msg_type: u32,
        payload: &T,
    ) -> Result<(), IpcError> {
        syscall::handle_send(self.fd(), handles).map_err(IpcError::Syscall)?;
        self.send(msg_type, payload)
    }

    /// Take the batch the peer sent with the frame just received.
    ///
    /// Never blocks. An empty answer for a frame that promised handles is a
    /// peer that sent them out of order, which is a protocol error and not
    /// something to wait for.
    pub fn recv_handles(
        &self,
        out: &mut [RawHandle; syscall::MAX_TRANSFER_HANDLES],
    ) -> Result<usize, SyscallError> {
        syscall::handle_recv(self.fd(), out)
    }

    /// The `N` handles this frame's message type says travel with it.
    ///
    /// `None` for any other count, with whatever did arrive closed rather than
    /// leaked: a short batch is a peer that sent its frame first, a long one is
    /// a protocol this endpoint does not speak, and neither is something to
    /// wait for. Every protocol here sends a fixed number per message type, so
    /// this is the shape every caller wants.
    pub fn recv_handles_exact<const N: usize>(&self) -> Option<[RawHandle; N]> {
        recv_handles_exact(self.fd())
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

pub fn send<T: IpcPayload>(fd: RawHandle, msg_type: u32, payload: &T) -> Result<(), IpcError> {
    let header = IpcHeader::frame(msg_type, core::mem::size_of::<T>())?;
    write_all(fd, &header.to_wire())?;
    write_all(fd, as_bytes(payload))
}

pub fn signal(fd: RawHandle, msg_type: u32) -> Result<(), IpcError> {
    write_all(fd, &IpcHeader { msg_type, len: 0 }.to_wire())
}

pub fn send_bytes(fd: RawHandle, msg_type: u32, data: &[u8]) -> Result<(), IpcError> {
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
pub fn try_send<T: IpcPayload>(fd: RawHandle, msg_type: u32, payload: &T) -> Result<(), TrySendError> {
    const { assert!(core::mem::size_of::<T>() <= MAX_TYPED_PAYLOAD) };
    let size = core::mem::size_of::<T>();
    let mut frame = [0u8; IpcHeader::WIRE_SIZE + MAX_TYPED_PAYLOAD];
    let header = IpcHeader::frame(msg_type, size).map_err(|_| TrySendError::TooLarge)?;
    frame[..IpcHeader::WIRE_SIZE].copy_from_slice(&header.to_wire());
    frame[IpcHeader::WIRE_SIZE..IpcHeader::WIRE_SIZE + size].copy_from_slice(as_bytes(payload));
    write_whole(fd, &frame[..IpcHeader::WIRE_SIZE + size])
}

/// [`try_send`] preceded by the move of the handles its payload describes.
pub fn try_send_with_handles<T: IpcPayload>(
    fd: RawHandle,
    handles: &[RawHandle],
    msg_type: u32,
    payload: &T,
) -> Result<(), TrySendError> {
    syscall::handle_send(fd, handles).map_err(TrySendError::Syscall)?;
    try_send(fd, msg_type, payload)
}

/// The `N` handles the frame just read off `fd` says travel with it.
///
/// See [`Connection::recv_handles_exact`]. This is the spelling for a server
/// that buffers frames and dispatches them by handle rather than holding the
/// `Connection` — the compositor reads every client this way.
pub fn recv_handles_exact<const N: usize>(fd: RawHandle) -> Option<[RawHandle; N]> {
    const { assert!(N <= syscall::MAX_TRANSFER_HANDLES) };
    let mut batch = [toyos_abi::HANDLE_INVALID; syscall::MAX_TRANSFER_HANDLES];
    let n = syscall::handle_recv(fd, &mut batch).ok()?;
    if n != N {
        for h in &batch[..n] {
            syscall::close(*h);
        }
        return None;
    }
    let mut out = [toyos_abi::HANDLE_INVALID; N];
    out.copy_from_slice(&batch[..N]);
    Some(out)
}

/// [`signal`] without blocking. A bare header always fits in one write.
pub fn try_signal(fd: RawHandle, msg_type: u32) -> Result<(), TrySendError> {
    write_whole(fd, &IpcHeader { msg_type, len: 0 }.to_wire())
}

/// [`send_bytes`] without blocking.
///
/// The frame buffer is [`MAX_FRAME_LEN`] on the stack, so this is for the
/// occasional large message — a clipboard paste — not for a per-frame path.
pub fn try_send_bytes(fd: RawHandle, msg_type: u32, data: &[u8]) -> Result<(), TrySendError> {
    let mut frame = [0u8; IpcHeader::WIRE_SIZE + MAX_FRAME_LEN as usize];
    let header = IpcHeader::frame(msg_type, data.len()).map_err(|_| TrySendError::TooLarge)?;
    frame[..IpcHeader::WIRE_SIZE].copy_from_slice(&header.to_wire());
    frame[IpcHeader::WIRE_SIZE..IpcHeader::WIRE_SIZE + data.len()].copy_from_slice(data);
    write_whole(fd, &frame[..IpcHeader::WIRE_SIZE + data.len()])
}

pub fn recv_header(fd: RawHandle) -> Result<IpcHeader, IpcError> {
    let mut bytes = [0u8; IpcHeader::WIRE_SIZE];
    read_exact(fd, &mut bytes)?;
    IpcHeader::from_wire(
        u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    )
}

pub fn recv_payload<T: IpcPayload>(fd: RawHandle, header: &IpcHeader) -> Result<T, IpcError> {
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
pub fn recv<T: IpcPayload>(fd: RawHandle) -> Result<(u32, T), IpcError> {
    let header = recv_header(fd)?;
    let payload = recv_payload(fd, &header)?;
    Ok((header.msg_type, payload))
}

/// Receive raw bytes. Returns the number of valid bytes read.
pub fn recv_bytes(fd: RawHandle, header: &IpcHeader, buf: &mut [u8]) -> Result<usize, IpcError> {
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

/// What one [`FrameRx::pump`] found.
#[derive(Clone, Copy, Debug)]
pub enum RxStep {
    /// The fd has nothing more right now.
    Idle,
    /// A whole frame is buffered; its payload is [`FrameRx::payload`].
    Frame { msg_type: u32, payload_len: usize },
    /// The peer hung up, or its fd is gone.
    Eof,
    /// A frame no protocol here can produce. Nothing after it can be located,
    /// so there is nothing to resynchronise to.
    Malformed,
}

/// One peer's inbound framing, over non-blocking reads only.
///
/// **A server never reads a client with a blocking read.** That is the whole
/// point of this type: [`recv_header`] and [`recv_payload`] park the caller
/// until the peer sends the bytes it promised, which hands a client the
/// decision of when the server runs again. Here a peer that stops halfway
/// through a frame costs a buffer instead of the event loop.
///
/// `KEEP` is how much of a payload the caller wants kept; anything past it is
/// read and discarded, so a peer cannot make the server hold what it will not
/// look at. A protocol whose messages are all bare headers takes `KEEP = 0`.
pub struct FrameRx<const KEEP: usize> {
    header: [u8; IpcHeader::WIRE_SIZE],
    buf: [u8; KEEP],
    len: usize,
    state: RxState,
}

enum RxState {
    Header,
    Payload { msg_type: u32, keep: usize, skip: u32 },
    Skip { msg_type: u32, kept: usize, remaining: u32 },
}

impl<const KEEP: usize> Default for FrameRx<KEEP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const KEEP: usize> FrameRx<KEEP> {
    pub const fn new() -> Self {
        Self {
            header: [0; IpcHeader::WIRE_SIZE],
            buf: [0; KEEP],
            len: 0,
            state: RxState::Header,
        }
    }

    /// The payload of the frame the last [`RxStep::Frame`] announced.
    pub fn payload(&self, len: usize) -> &[u8] {
        &self.buf[..len]
    }

    /// Read what is there, and report the first thing that happened.
    ///
    /// At most one frame per call: the caller has other peers to serve, and a
    /// peer that always has another frame ready must not be able to keep the
    /// rest from being served.
    pub fn pump(&mut self, conn: &Connection) -> RxStep {
        loop {
            match self.state {
                RxState::Header => {
                    match fill(conn, &mut self.header, &mut self.len) {
                        Fill::Idle => return RxStep::Idle,
                        Fill::Eof => return RxStep::Eof,
                        Fill::Filled => {}
                    }
                    let msg_type = u32::from_ne_bytes(self.header[0..4].try_into().unwrap());
                    let wire_len = u32::from_ne_bytes(self.header[4..8].try_into().unwrap());
                    // The bound belongs to `IpcHeader`, so it is asked rather
                    // than restated: a length past `MAX_FRAME_LEN` is a frame
                    // this endpoint refuses to describe, not a long one.
                    let Ok(header) = IpcHeader::from_wire(msg_type, wire_len) else {
                        return RxStep::Malformed;
                    };
                    self.len = 0;
                    let keep = (header.len() as usize).min(KEEP);
                    let skip = header.len() - keep as u32;
                    self.state = RxState::Payload { msg_type, keep, skip };
                }
                RxState::Payload { msg_type, keep, skip } => {
                    if keep > 0 {
                        match fill(conn, &mut self.buf[..keep], &mut self.len) {
                            Fill::Idle => return RxStep::Idle,
                            Fill::Eof => return RxStep::Eof,
                            Fill::Filled => {}
                        }
                    }
                    self.len = 0;
                    self.state = RxState::Skip { msg_type, kept: keep, remaining: skip };
                }
                RxState::Skip { msg_type, kept, remaining } => {
                    if remaining > 0 {
                        let mut sink = [0u8; 128];
                        let want = (remaining as usize).min(sink.len());
                        let mut got = 0;
                        match fill(conn, &mut sink[..want], &mut got) {
                            Fill::Idle => {
                                self.state =
                                    RxState::Skip { msg_type, kept, remaining: remaining - got as u32 };
                                return RxStep::Idle;
                            }
                            Fill::Eof => return RxStep::Eof,
                            Fill::Filled => {
                                self.state =
                                    RxState::Skip { msg_type, kept, remaining: remaining - want as u32 };
                                continue;
                            }
                        }
                    }
                    self.state = RxState::Header;
                    return RxStep::Frame { msg_type, payload_len: kept };
                }
            }
        }
    }
}

enum Fill {
    Filled,
    Idle,
    Eof,
}

/// Fill `buf` from `*len` onwards without blocking, carrying `*len` forward so
/// the next call resumes where this one stopped.
fn fill(conn: &Connection, buf: &mut [u8], len: &mut usize) -> Fill {
    while *len < buf.len() {
        match conn.read_nonblock(&mut buf[*len..]) {
            Ok(0) => return Fill::Eof,
            Ok(n) => *len += n,
            Err(SyscallError::WouldBlock) => return Fill::Idle,
            // The fd itself is gone, which is the same hang-up seen from the
            // other side of the same race.
            Err(_) => return Fill::Eof,
        }
    }
    Fill::Filled
}

fn as_bytes<T: IpcPayload>(val: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts(val as *const T as *const u8, core::mem::size_of::<T>()) }
}

fn skip(fd: RawHandle, mut remaining: usize) -> Result<(), IpcError> {
    let mut buf = [0u8; 128];
    while remaining > 0 {
        let chunk = remaining.min(buf.len());
        read_exact(fd, &mut buf[..chunk])?;
        remaining -= chunk;
    }
    Ok(())
}

fn read_exact(fd: RawHandle, buf: &mut [u8]) -> Result<(), IpcError> {
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
fn write_whole(fd: RawHandle, buf: &[u8]) -> Result<(), TrySendError> {
    match syscall::write_nonblock(fd, buf) {
        Ok(n) if n == buf.len() => Ok(()),
        Ok(_) => Err(TrySendError::Full),
        Err(SyscallError::WouldBlock) => Err(TrySendError::Full),
        Err(e) => Err(TrySendError::Syscall(e)),
    }
}

fn write_all(fd: RawHandle, buf: &[u8]) -> Result<(), IpcError> {
    let mut offset = 0;
    while offset < buf.len() {
        let n = syscall::write(fd, &buf[offset..]).map_err(IpcError::Syscall)?;
        offset += n;
    }
    Ok(())
}
