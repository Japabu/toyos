//! Asking `/bin/init` to start a declared program.
//!
//! **`SYS_SPAWN` can only give a child what the caller holds, and that is not
//! enough.** doom is started from a shell and doom is to hold sound; the shell
//! is not. Under inheritance alone doom could hold sound only if every shell
//! did. So init serves `launcher`: the caller names a `[programs]` key, init
//! looks up what the manifest says that program holds, builds it out of the
//! connectors and claims *init* holds, and spawns it.
//!
//! **The asker's authority is one connector**, and the policy is the manifest.
//! A launch adds nothing to what the caller could do itself: the stdio handles
//! and the extra connectors travel from the caller's own table, which is the
//! same bound `SYS_SPAWN` has. What it *adds* is that the child's namespace is
//! its own manifest row rather than a narrowing of its parent's.
//!
//! The wire is a single frame plus one handle batch, and this module is both
//! halves of it — std's `Command` encodes and `/bin/init` decodes.

use crate::ipc::{Connection, IpcError};
use crate::RawHandle;

/// Ask init to start a program. Carries the request blob below; the stdio
/// handles and the extra connectors travel in the batch beside it.
pub const MSG_LAUNCH: u32 = 1;
/// It started. One `Process` handle travels with this.
pub const MSG_LAUNCHED: u32 = 2;
/// No `[programs]` row names that program, so init has nothing to build its
/// authority from. The caller spawns it directly instead, which gets
/// inheritance — the right answer for a binary the image did not declare.
pub const MSG_NOT_DECLARED: u32 = 3;
/// It is declared and did not start: no such file, a full table, a refused
/// endowment. The reason is in init's log, not in this frame — a caller can do
/// nothing differently about any of them.
pub const MSG_REFUSED: u32 = 4;

/// Connectors one launch may carry from its caller.
///
/// Policy on the primitive, refused by name. Five and not more because the
/// batch also carries the three stdio handles and
/// [`MAX_TRANSFER_HANDLES`](toyos_abi::syscall::MAX_TRANSFER_HANDLES) is eight:
/// a launch is one crossing, and splitting it into two would put a child's
/// authority in two batches that can arrive apart.
pub const MAX_LAUNCH_EXTRAS: usize = 5;

/// Slots a launch may install in the child: stdin, stdout and stderr, and a
/// caller that wants anything else in a child's table spawns it directly.
pub const MAX_LAUNCH_SLOTS: usize = 3;

const HEADER: usize = 32;

/// What a caller asks init to start.
///
/// `argv` and `env` are exactly the blobs `SYS_SPAWN` takes, so the launcher
/// path and the direct path build one thing rather than two.
pub struct Launch<'a> {
    /// The program's path, exactly as the caller resolved it.
    ///
    /// **Not a `[programs]` key**, because `/bin/ls` is a symlink to
    /// `/bin/toybox` and the row that says what an applet may hold is
    /// `toybox`'s. init resolves the path to a key; the caller does not parse
    /// the manifest and there is deliberately no second reader of it.
    pub program: &'a str,
    /// NUL-separated, `argv[0]` first.
    pub argv: &'a [u8],
    /// `KEY=VALUE\0` repeated. **Carried rather than inherited**: without it a
    /// launched child would get init's environment, and `cd /tmp && ls` would
    /// list `/`.
    pub env: &'a [u8],
    pub cwd: &'a str,
    /// `(name, connector)` the caller transfers into the child's namespace, on
    /// top of the manifest's. This is how a terminal gives its shell the
    /// `surface` port it made — a name init cannot know, because there is one
    /// per terminal.
    pub extras: &'a [(&'a str, RawHandle)],
    /// `(child slot, handle)`, at most [`MAX_LAUNCH_SLOTS`] of them. The
    /// handles are **moved**, so a caller passing on its own stdout duplicates
    /// first — which is the same rule `SpawnArgs`'s two vectors state, with the
    /// duplication moved to the caller because a launch has only the one verb.
    pub slots: &'a [(u32, RawHandle)],
}

/// Why a request would not go on the wire.
#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// More than [`MAX_LAUNCH_EXTRAS`], or more than [`MAX_LAUNCH_SLOTS`].
    TooMany,
    /// The blobs do not fit one frame.
    TooLarge,
}

impl Launch<'_> {
    /// The handles this launch moves, slots first.
    pub fn handles(&self) -> ([RawHandle; MAX_LAUNCH_EXTRAS + MAX_LAUNCH_SLOTS], usize) {
        let mut out = [RawHandle(0); MAX_LAUNCH_EXTRAS + MAX_LAUNCH_SLOTS];
        let mut n = 0;
        for (_, handle) in self.slots {
            out[n] = *handle;
            n += 1;
        }
        for (_, handle) in self.extras {
            out[n] = *handle;
            n += 1;
        }
        (out, n)
    }

    /// Write the request blob into `buf`, answering its length.
    pub fn encode(&self, buf: &mut [u8]) -> Result<usize, EncodeError> {
        if self.extras.len() > MAX_LAUNCH_EXTRAS || self.slots.len() > MAX_LAUNCH_SLOTS {
            return Err(EncodeError::TooMany);
        }
        let names_len: usize = self.extras.iter().map(|(n, _)| n.len() + 1).sum();
        let slots_len = self.slots.len() * 4;
        let total = HEADER
            + slots_len
            + self.program.len()
            + self.argv.len()
            + self.env.len()
            + self.cwd.len()
            + names_len;
        if total > buf.len() || total > crate::ipc::MAX_FRAME_LEN as usize {
            return Err(EncodeError::TooLarge);
        }
        let lens = [
            self.slots.len() as u32,
            self.extras.len() as u32,
            slots_len as u32,
            self.program.len() as u32,
            self.argv.len() as u32,
            self.env.len() as u32,
            self.cwd.len() as u32,
            names_len as u32,
        ];
        for (i, v) in lens.iter().enumerate() {
            buf[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let mut at = HEADER;
        let mut put = |bytes: &[u8], at: &mut usize| {
            buf[*at..*at + bytes.len()].copy_from_slice(bytes);
            *at += bytes.len();
        };
        for (slot, _) in self.slots {
            put(&slot.to_le_bytes(), &mut at);
        }
        put(self.program.as_bytes(), &mut at);
        put(self.argv, &mut at);
        put(self.env, &mut at);
        put(self.cwd.as_bytes(), &mut at);
        for (name, _) in self.extras {
            put(name.as_bytes(), &mut at);
            put(&[0], &mut at);
        }
        Ok(total)
    }
}

/// A decoded request, borrowing the frame it arrived in.
///
/// **Every field is a peer's claim about itself**, so nothing here is trusted:
/// a length that does not fit refuses the whole frame, and the program name
/// resolves against the manifest or does not resolve at all.
pub struct Request<'a> {
    pub program: &'a str,
    pub argv: &'a [u8],
    pub env: &'a [u8],
    pub cwd: &'a str,
    pub extra_count: usize,
    slots: &'a [u8],
    names: &'a [u8],
}

impl<'a> Request<'a> {
    pub fn decode(buf: &'a [u8]) -> Option<Self> {
        if buf.len() < HEADER {
            return None;
        }
        let field = |i: usize| {
            u32::from_le_bytes([buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2], buf[i * 4 + 3]])
                as usize
        };
        let (slot_count, extra_count) = (field(0), field(1));
        let (slots_len, program_len, argv_len, env_len, cwd_len, names_len) =
            (field(2), field(3), field(4), field(5), field(6), field(7));
        if extra_count > MAX_LAUNCH_EXTRAS
            || slot_count > MAX_LAUNCH_SLOTS
            || slots_len != slot_count * 4
        {
            return None;
        }
        let total = HEADER
            .checked_add(slots_len)?
            .checked_add(program_len)?
            .checked_add(argv_len)?
            .checked_add(env_len)?
            .checked_add(cwd_len)?
            .checked_add(names_len)?;
        if total > buf.len() {
            return None;
        }
        let mut at = HEADER;
        let take = |len: usize, at: &mut usize| {
            let slice = &buf[*at..*at + len];
            *at += len;
            slice
        };
        let slots = take(slots_len, &mut at);
        let program = core::str::from_utf8(take(program_len, &mut at)).ok()?;
        let argv = take(argv_len, &mut at);
        let env = take(env_len, &mut at);
        let cwd = core::str::from_utf8(take(cwd_len, &mut at)).ok()?;
        let names = take(names_len, &mut at);
        if names.iter().filter(|&&b| b == 0).count() != extra_count {
            return None;
        }
        Some(Self { program, argv, env, cwd, extra_count, slots, names })
    }

    /// The child slots the first handles of the batch are for, in order.
    pub fn slot_numbers(&self) -> impl Iterator<Item = u32> + '_ {
        self.slots.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
    }

    pub fn slot_count(&self) -> usize {
        self.slots.len() / 4
    }

    /// The extra connectors' names, in the order their handles arrive.
    pub fn extra_names(&self) -> impl Iterator<Item = &'a str> {
        self.names.split(|&b| b == 0).filter(|s| !s.is_empty()).filter_map(|s| {
            core::str::from_utf8(s).ok()
        })
    }
}

/// Why a launch did not answer, and — the part that matters — whether the
/// handles it was going to carry are still the caller's.
///
/// **A send moves them.** `SYS_HANDLE_SEND` takes them out of the sender's
/// table, so a caller that closed them after a refusal would be closing handles
/// it no longer holds — which, under the bad-handle policy, is the caller
/// exiting. Which side owns them is therefore not a detail of the error but the
/// whole of what the caller needs from it.
pub enum LaunchError {
    /// Nothing left this process. The handles are still here to close.
    NotSent(IpcError),
    /// The handles moved and the answer did not come back. They are the
    /// launcher's to release now.
    Sent(IpcError),
}

/// What a launch answered.
pub enum Outcome {
    /// The handle the caller now holds for the child.
    Started(RawHandle),
    /// Not a `[programs]` key. The caller spawns it directly.
    NotDeclared,
    /// Declared, and it did not start.
    Refused,
}

/// Send one launch and read its answer.
///
/// The handles go before the frame that announces them, which is
/// [`Connection::send_with_handles`]'s whole rule — and the `Process` handle
/// comes back the same way.
pub fn launch(conn: &Connection, request: &Launch<'_>) -> Result<Outcome, LaunchError> {
    let mut buf = [0u8; crate::ipc::MAX_FRAME_LEN as usize];
    let len = request
        .encode(&mut buf)
        .map_err(|_| LaunchError::NotSent(IpcError::TooLarge))?;
    let (handles, count) = request.handles();
    conn.send_bytes_with_handles(&handles[..count], MSG_LAUNCH, &buf[..len])
        .map_err(LaunchError::Sent)?;
    let header = conn.recv_header().map_err(LaunchError::Sent)?;
    match header.msg_type {
        MSG_LAUNCHED => match conn.recv_handles_exact::<1>() {
            Some([process]) => Ok(Outcome::Started(process)),
            None => Err(LaunchError::Sent(IpcError::Malformed)),
        },
        MSG_NOT_DECLARED => Ok(Outcome::NotDeclared),
        MSG_REFUSED => Ok(Outcome::Refused),
        _ => Err(LaunchError::Sent(IpcError::Malformed)),
    }
}
