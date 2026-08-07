use crate::fd::{Descriptor, FramebufferInfo};
use crate::{keyboard, mouse};
use crate::process::Pid;
use crate::shared_memory;
use crate::sync::Lock;
pub use toyos_abi::syscall::DeviceType;

/// The wire number `SYS_OPEN_DEVICE` carries, decoded once.
pub fn class_of(raw: u64) -> Option<DeviceType> {
    match raw {
        0 => Some(DeviceType::Keyboard),
        1 => Some(DeviceType::Mouse),
        2 => Some(DeviceType::Framebuffer),
        3 => Some(DeviceType::Nic),
        4 => Some(DeviceType::Audio),
        _ => None,
    }
}

static KEYBOARD_OWNER: Lock<Option<Pid>> = Lock::new(None);
static MOUSE_OWNER: Lock<Option<Pid>> = Lock::new(None);
static FRAMEBUFFER_OWNER: Lock<Option<Pid>> = Lock::new(None);
static NIC_OWNER: Lock<Option<Pid>> = Lock::new(None);
static AUDIO_OWNER: Lock<Option<Pid>> = Lock::new(None);
static FB_INFO: Lock<Option<FramebufferInfo>> = Lock::new(None);

fn owner_of(class: DeviceType) -> &'static Lock<Option<Pid>> {
    match class {
        DeviceType::Keyboard => &KEYBOARD_OWNER,
        DeviceType::Mouse => &MOUSE_OWNER,
        DeviceType::Framebuffer => &FRAMEBUFFER_OWNER,
        DeviceType::Nic => &NIC_OWNER,
        DeviceType::Audio => &AUDIO_OWNER,
    }
}

/// The claim itself, as a value.
///
/// Not `Clone` and not `Copy`, and the field is private, so the only ways to
/// obtain one are [`Claim::acquire`] and moving an existing one. That is the
/// exclusivity: at most one `Claim` per class can exist at a time, and the
/// compiler — not a check in `dup` — is what says so. `Descriptor` therefore
/// cannot implement `Clone` either, which is where the rule reaches userland
/// (`Descriptor::duplicate`).
///
/// `capability-handles-spec.md` §6.5 reaches the same shape from the other
/// end: a claim handle carries no DUP right, and TRANSFER moves it whole.
pub struct Claim {
    class: DeviceType,
}

impl Claim {
    /// Take the class for `pid`, or say who has it.
    fn acquire(class: DeviceType, pid: Pid) -> Result<Self, ClaimError> {
        let mut owner = owner_of(class).lock();
        if owner.is_some() {
            return Err(ClaimError::Owned);
        }
        *owner = Some(pid);
        Ok(Self { class })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        *owner_of(self.class).lock() = None;
    }
}

pub fn set_framebuffer_info(info: FramebufferInfo) {
    // A relative pointer accumulates into a square 0..32767 space that this
    // geometry is what gets mapped onto, so its per-axis scale is a function of
    // the screen and has to follow a mode change.
    crate::mouse::set_screen(info.width, info.height);
    *FB_INFO.lock() = Some(info);
}

/// Why a claim did not succeed.
///
/// A daemon's whole degradation decision turns on this: "this machine has no
/// sound card" is a machine, and exiting is right; "another process holds the
/// sound card" is a conflict, and exiting silently turns it into a session
/// with no audio and no record of why. One `None` could not tell them apart,
/// so soundd's and netd's "no device on this machine" line was an assertion
/// rather than a check.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClaimError {
    /// Another process holds the claim.
    Owned,
    /// This machine has no such device — no driver ever registered one.
    Absent,
    /// The device exists and is free, but its buffers could not be granted.
    GrantFailed,
}

/// Try to claim exclusive access to a device.
///
/// Every `?` past the `acquire` gives the class straight back: the `Claim` is
/// on this stack frame, so a failure cannot leave the device owned by a
/// process that was refused it. The three hand-written `*owner = None`
/// rollbacks this replaced were what an added grant would have had to
/// remember.
pub fn try_claim(class: DeviceType, pid: Pid) -> Result<Descriptor, ClaimError> {
    // Availability is decided before the claim, so a second claimant of an
    // absent device is told `Absent` and not `Owned` — the distinction soundd
    // and netd degrade on.
    match class {
        DeviceType::Keyboard => {
            let claim = Claim::acquire(class, pid)?;
            // Whatever was typed while nobody held the device belongs to
            // nobody. Delivering it to whoever claims next hands one program
            // another's keystrokes, and a compositor restarted mid-sentence
            // would open with the tail of what was being typed into the one
            // that died.
            keyboard::discard_queued();
            Ok(Descriptor::Keyboard(claim))
        }
        DeviceType::Mouse => {
            let claim = Claim::acquire(class, pid)?;
            mouse::discard_queued();
            Ok(Descriptor::Mouse(claim))
        }
        DeviceType::Framebuffer => {
            let info = (*FB_INFO.lock()).ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class, pid)?;
            for &token in &info.token {
                grant(token, pid)?;
            }
            grant(info.cursor_token, pid)?;
            crate::drivers::panic_console::screen_claimed_by_userland();
            Ok(Descriptor::Framebuffer(claim, info))
        }
        DeviceType::Nic => {
            let info = crate::net::nic_info().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class, pid)?;
            grant(info.dma_token, pid)?;
            Ok(Descriptor::Nic(claim, info))
        }
        DeviceType::Audio => {
            let info = crate::audio::audio_info().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class, pid)?;
            grant(info.dma_token, pid)?;
            Ok(Descriptor::Audio { claim, info, info_read: false })
        }
    }
}

fn grant(token: u32, pid: Pid) -> Result<(), ClaimError> {
    shared_memory::grant_kernel(shared_memory::SharedToken::from_raw(token), pid)
        .map_err(|_| ClaimError::GrantFailed)
}

/// True when `pid` currently holds the claim on `class`. Syscalls that drive a
/// claimed device gate on this — a claim is what makes a process the device's
/// owner, so it is also what makes it allowed to reconfigure it.
pub fn is_owner(class: DeviceType, pid: Pid) -> bool {
    *owner_of(class).lock() == Some(pid)
}
