use alloc::sync::Arc;

use crate::object::device::{DeviceClaim, DeviceInfo};
use crate::{keyboard, mouse};
use toyos_abi::FramebufferInfo;
use crate::process::Pid;
use crate::shared_memory;
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
static FB_INFO: Lock<Option<FramebufferInfo>> = Lock::new(None);

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
            let info = (*FB_INFO.lock()).ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class)?;
            crate::drivers::panic_console::screen_claimed_by_userland();
            Ok(DeviceClaim::new(class, DeviceInfo::Framebuffer(info), claim))
        }
        DeviceType::Nic => {
            let info = crate::net::nic_info().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class)?;
            Ok(DeviceClaim::new(class, DeviceInfo::Nic(info), claim))
        }
        DeviceType::HdaAudio => {
            let info = crate::drivers::hda::info().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class)?;
            Ok(DeviceClaim::new(class, DeviceInfo::Hda(info), claim))
        }
        DeviceType::VirtioSound => {
            let info = crate::drivers::virtio_sound::info().ok_or(ClaimError::Absent)?;
            let claim = Claim::acquire(class)?;
            Ok(DeviceClaim::new(class, DeviceInfo::VirtioSound(info), claim))
        }
    }
}

/// Let `pid` map every kernel buffer a claim's description names.
///
/// **Granted where the description is read, not where the claim is minted.**
/// init mints every claim and holds none of them: it endows each one to the
/// program the manifest gives it, and that program is the one that has to map
/// the scanout or the DMA region. Granting at mint time would grant to init,
/// and the holder would read an address it is not allowed to touch.
///
/// A claim admits one handle, so at most one process at a time can reach this —
/// and the grants a previous holder collected are released with the region's
/// own accounting when that process exits.
pub fn grant_buffers(info: &DeviceInfo, pid: Pid) {
    let tokens: &[u32] = match info {
        DeviceInfo::Events => &[],
        DeviceInfo::Framebuffer(fb) => &[fb.token[0], fb.token[1], fb.cursor_token],
        DeviceInfo::Nic(nic) => &[nic.dma_token],
        DeviceInfo::Hda(hda) => &[hda.pcm_token],
        DeviceInfo::VirtioSound(vs) => &[vs.dma_token],
    };
    for &token in tokens {
        // A refusal here is the kernel disagreeing with itself about a region
        // it created, and the holder would read a description naming memory it
        // cannot map. Say which token rather than hand it over anyway.
        shared_memory::grant_kernel(shared_memory::SharedToken::from_raw(token), pid)
            .unwrap_or_else(|e| panic!("device buffer {token} cannot be granted: {e:?}"));
    }
}
