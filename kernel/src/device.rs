use crate::fd::{Descriptor, FramebufferInfo};
use crate::process::Pid;
use crate::shared_memory;
use crate::sync::Lock;

pub const DEVICE_KEYBOARD: u64 = 0;
pub const DEVICE_MOUSE: u64 = 1;
pub const DEVICE_FRAMEBUFFER: u64 = 2;
pub const DEVICE_NIC: u64 = 3;
pub const DEVICE_AUDIO: u64 = 4;

static KEYBOARD_OWNER: Lock<Option<Pid>> = Lock::new(None);
static MOUSE_OWNER: Lock<Option<Pid>> = Lock::new(None);
static FRAMEBUFFER_OWNER: Lock<Option<Pid>> = Lock::new(None);
static NIC_OWNER: Lock<Option<Pid>> = Lock::new(None);
static AUDIO_OWNER: Lock<Option<Pid>> = Lock::new(None);
static FB_INFO: Lock<Option<FramebufferInfo>> = Lock::new(None);

pub fn set_framebuffer_info(info: FramebufferInfo) {
    *FB_INFO.lock() = Some(info);
}

/// Try to claim exclusive access to a device. Returns the Descriptor if unclaimed.
pub fn try_claim(device_type: u64, pid: Pid) -> Option<Descriptor> {
    match device_type {
        DEVICE_KEYBOARD => {
            let mut owner = KEYBOARD_OWNER.lock();
            if owner.is_some() {
                return None;
            }
            *owner = Some(pid);
            Some(Descriptor::Keyboard)
        }
        DEVICE_MOUSE => {
            let mut owner = MOUSE_OWNER.lock();
            if owner.is_some() {
                return None;
            }
            *owner = Some(pid);
            Some(Descriptor::Mouse)
        }
        DEVICE_FRAMEBUFFER => {
            let mut owner = FRAMEBUFFER_OWNER.lock();
            if owner.is_some() {
                return None;
            }
            let info = (*FB_INFO.lock())?;
            *owner = Some(pid);
            // Grant GPU buffer and cursor tokens to the claiming process
            for &token in &info.token {
                if shared_memory::grant_kernel(shared_memory::SharedToken::from_raw(token), pid).is_err() {
                    *owner = None;
                    return None;
                }
            }
            if shared_memory::grant_kernel(shared_memory::SharedToken::from_raw(info.cursor_token), pid).is_err() {
                *owner = None;
                return None;
            }
            Some(Descriptor::Framebuffer(info))
        }
        DEVICE_NIC => {
            let mut owner = NIC_OWNER.lock();
            if owner.is_some() {
                return None;
            }
            let info = crate::net::nic_info()?;
            *owner = Some(pid);
            // Grant DMA buffer tokens to the claiming process
            if shared_memory::grant_kernel(shared_memory::SharedToken::from_raw(info.dma_token), pid).is_err() {
                *owner = None;
                return None;
            }
            Some(Descriptor::Nic(info))
        }
        DEVICE_AUDIO => {
            let mut owner = AUDIO_OWNER.lock();
            if owner.is_some() {
                return None;
            }
            let info = crate::audio::audio_info()?;
            *owner = Some(pid);
            if shared_memory::grant_kernel(shared_memory::SharedToken::from_raw(info.dma_token), pid).is_err() {
                *owner = None;
                return None;
            }
            Some(Descriptor::Audio { info, info_read: false })
        }
        _ => None,
    }
}

/// Release a device owned by the given PID.
pub fn release(device_type: u64, pid: Pid) {
    let mut owner = match device_type {
        DEVICE_KEYBOARD => KEYBOARD_OWNER.lock(),
        DEVICE_MOUSE => MOUSE_OWNER.lock(),
        DEVICE_FRAMEBUFFER => FRAMEBUFFER_OWNER.lock(),
        DEVICE_NIC => NIC_OWNER.lock(),
        DEVICE_AUDIO => AUDIO_OWNER.lock(),
        _ => return,
    };
    if *owner == Some(pid) {
        *owner = None;
    }
}

/// Release a device descriptor, determining the type from the descriptor variant.
pub fn release_descriptor(desc: &Descriptor, pid: Pid) {
    match desc {
        Descriptor::Keyboard => release(DEVICE_KEYBOARD, pid),
        Descriptor::Mouse => release(DEVICE_MOUSE, pid),
        Descriptor::Framebuffer(_) => release(DEVICE_FRAMEBUFFER, pid),
        Descriptor::Nic(_) => release(DEVICE_NIC, pid),
        Descriptor::Audio { .. } => release(DEVICE_AUDIO, pid),
        _ => {}
    }
}

