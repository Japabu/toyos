use alloc::sync::Arc;

use crate::object::device::{DeviceClaim, DeviceInfo, FramebufferBuffers};
use crate::object::shm::Region;
use crate::{keyboard, mouse};
use toyos_abi::FramebufferInfo;
use crate::sync::Lock;
pub use toyos_abi::syscall::DeviceType;

/// Whether each class is claimed — and deliberately not by whom.
///
/// This was six `Lock<Option<Pid>>` statics, and the pid in them existed for
/// one caller: `is_owner`, which nine device syscalls asked before driving the
/// hardware. That is designation by ambient property, and every one of those
/// nine takes the claim handle now, so the only question left is the one
/// exclusivity actually needs.
static TAKEN: [Lock<bool>; DeviceType::ALL.len()] =
    [const { Lock::new(false) }; DeviceType::ALL.len()];
static FB_INFO: Lock<Option<Screen>> = Lock::new(None);

/// What the display driver published, as a claim needs it.
///
/// The regions rather than objects: a claim mints its own, because a
/// `SharedMemObject` whose handle count has reached zero is retired and the
/// screen outlives whichever process was holding it.
#[derive(Clone)]
pub struct Screen {
    pub info: FramebufferInfo,
    pub scanout: [Region; 2],
    pub cursor: Region,
}

fn taken(class: DeviceType) -> &'static Lock<bool> {
    &TAKEN[DeviceType::ALL
        .iter()
        .position(|c| *c == class)
        .expect("`DeviceType::ALL` names every class")]
}

/// The claim itself, as a value.
///
/// Not `Clone` and not `Copy`, and the field is private, so the only ways to
/// obtain one are [`Claim::acquire`] and moving an existing one. That is the
/// exclusivity: at most one `Claim` per class can exist at a time, and the
/// compiler — not a check in `dup` — is what says so.
///
/// The rule reaches userland through the object that holds one: a
/// [`DeviceClaim`] is created without `Rights::DUP`, so at most one handle to
/// it can exist and a transfer moves it whole.
pub struct Claim {
    class: DeviceType,
}

impl Claim {
    /// Take the class, or say it is already held.
    fn acquire(class: DeviceType) -> Result<Self, ClaimError> {
        let mut held = taken(class).lock();
        if *held {
            return Err(ClaimError::Owned);
        }
        *held = true;
        Ok(Self { class })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        *taken(self.class).lock() = false;
    }
}

pub fn set_framebuffer_info(screen: Screen) {
    // A relative pointer accumulates into a square 0..32767 space that this
    // geometry is what gets mapped onto, so its per-axis scale is a function of
    // the screen and has to follow a mode change.
    crate::mouse::set_screen(screen.info.width, screen.info.height);
    *FB_INFO.lock() = Some(screen);
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
}

/// Try to claim exclusive access to a device.
///
/// The `Claim` is on this stack frame until the object takes it, so a failure
/// past the `acquire` cannot leave a class held by nobody.
pub fn try_claim(class: DeviceType) -> Result<Arc<DeviceClaim>, ClaimError> {
    // Availability is decided before the claim, so a second claimant of an
    // absent device is told `Absent` and not `Owned` — the distinction soundd
    // and netd degrade on.
    match class {
        DeviceType::Keyboard => {
            let claim = Claim::acquire(class)?;
            // Whatever was typed while nobody held the device belongs to
            // nobody. Delivering it to whoever claims next hands one program
            // another's keystrokes, and a compositor restarted mid-sentence
            // would open with the tail of what was being typed into the one
            // that died.
            keyboard::discard_queued();
            Ok(DeviceClaim::new(class, DeviceInfo::Events, claim))
        }
        DeviceType::Mouse => {
            let claim = Claim::acquire(class)?;
            mouse::discard_queued();
            Ok(DeviceClaim::new(class, DeviceInfo::Events, claim))
        }
        DeviceType::Framebuffer => {
            let screen = (*FB_INFO.lock()).clone().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class)?;
            crate::drivers::panic_console::screen_claimed_by_userland();
            Ok(DeviceClaim::new(class, framebuffer_info(screen), claim))
        }
        DeviceType::Nic => {
            let (info, dma) = crate::net::nic_info().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class)?;
            Ok(DeviceClaim::new(class, DeviceInfo::Nic(info, shm(dma)), claim))
        }
        DeviceType::HdaAudio => {
            let (info, pcm) = crate::drivers::hda::info().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class)?;
            Ok(DeviceClaim::new(class, DeviceInfo::Hda(info, shm(pcm)), claim))
        }
        DeviceType::VirtioSound => {
            let (info, dma) = crate::drivers::virtio_sound::info().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class)?;
            Ok(DeviceClaim::new(class, DeviceInfo::VirtioSound(info, shm(dma)), claim))
        }
    }
}

fn shm(region: Region) -> Arc<crate::object::shm::SharedMemObject> {
    crate::object::shm::SharedMemObject::over(region)
}

/// The description a framebuffer claim answers with, over freshly minted
/// buffer objects.
pub fn framebuffer_info(screen: Screen) -> DeviceInfo {
    let Screen { info, scanout: [front, back], cursor } = screen;
    DeviceInfo::Framebuffer(
        info,
        FramebufferBuffers { scanout: [shm(front), shm(back)], cursor: shm(cursor) },
    )
}
