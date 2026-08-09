//! A device claim, and the one console every kernel-spawned process starts on.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::device::{Claim, DeviceType};
use toyos_abi::FramebufferInfo;

use super::{Held, KObjectVariant, ObjectCore, ZeroHandles};

/// What the class answers when its holder reads it.
///
/// Keyboard and mouse answer with events rather than with a description, which
/// is why they have no arm here rather than an empty one.
pub enum DeviceInfo {
    Events,
    Framebuffer(FramebufferInfo),
    Nic(crate::net::NicInfo),
    Hda(toyos_abi::hda::HdaInfo),
    VirtioSound(toyos_abi::virtio_sound::VirtioSoundInfo),
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
    info: DeviceInfo,
    info_read: AtomicBool,
    reference: Held<Claim>,
}

impl DeviceClaim {
    pub fn new(class: DeviceType, info: DeviceInfo, claim: Claim) -> Arc<Self> {
        Arc::new(Self {
            core: Self::new_core(),
            class,
            info,
            info_read: AtomicBool::new(false),
            reference: Held::new(claim),
        })
    }

    pub fn class(&self) -> DeviceType {
        self.class
    }

    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    pub fn info_read(&self) -> bool {
        self.info_read.load(Ordering::Relaxed)
    }

    pub fn mark_info_read(&self) {
        self.info_read.store(true, Ordering::Relaxed);
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

/// The machine's serial console.
///
/// Not a [`DeviceClaim`] — a claim's whole content is exclusivity and every
/// kernel-spawned process holds one of these at once — and not a file: it has
/// no path, no cursor and no backing.
pub struct ConsoleObject {
    pub(super) core: ObjectCore,
}

impl ConsoleObject {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { core: Self::new_core() })
    }
}
