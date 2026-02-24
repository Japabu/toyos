use alloc::string::String;
use alloc::vec::Vec;
use crate::fd::{Descriptor, FramebufferInfo};
use crate::sync::SyncCell;

pub const DEVICE_KEYBOARD: u64 = 0;
pub const DEVICE_MOUSE: u64 = 1;
pub const DEVICE_FRAMEBUFFER: u64 = 2;

static KEYBOARD_OWNER: SyncCell<Option<u32>> = SyncCell::new(None);
static MOUSE_OWNER: SyncCell<Option<u32>> = SyncCell::new(None);
static FRAMEBUFFER_OWNER: SyncCell<Option<u32>> = SyncCell::new(None);
static FB_INFO: SyncCell<Option<FramebufferInfo>> = SyncCell::new(None);

static NAME_REGISTRY: SyncCell<Vec<(String, u32)>> = SyncCell::new(Vec::new());

pub fn set_framebuffer_info(info: FramebufferInfo) {
    *FB_INFO.get_mut() = Some(info);
}

/// Try to claim exclusive access to a device. Returns the Descriptor if unclaimed.
pub fn try_claim(device_type: u64, pid: u32) -> Option<Descriptor> {
    match device_type {
        DEVICE_KEYBOARD => {
            let owner = KEYBOARD_OWNER.get_mut();
            if owner.is_some() {
                return None;
            }
            *owner = Some(pid);
            Some(Descriptor::Keyboard)
        }
        DEVICE_MOUSE => {
            let owner = MOUSE_OWNER.get_mut();
            if owner.is_some() {
                return None;
            }
            *owner = Some(pid);
            Some(Descriptor::Mouse)
        }
        DEVICE_FRAMEBUFFER => {
            let owner = FRAMEBUFFER_OWNER.get_mut();
            if owner.is_some() {
                return None;
            }
            let info = (*FB_INFO.get())?;
            *owner = Some(pid);
            Some(Descriptor::Framebuffer(info))
        }
        _ => None,
    }
}

/// Release a device owned by the given PID.
pub fn release(device_type: u64, pid: u32) {
    let owner = match device_type {
        DEVICE_KEYBOARD => KEYBOARD_OWNER.get_mut(),
        DEVICE_MOUSE => MOUSE_OWNER.get_mut(),
        DEVICE_FRAMEBUFFER => FRAMEBUFFER_OWNER.get_mut(),
        _ => return,
    };
    if *owner == Some(pid) {
        *owner = None;
    }
}

/// Release a device descriptor, determining the type from the descriptor variant.
pub fn release_descriptor(desc: &Descriptor, pid: u32) {
    match desc {
        Descriptor::Keyboard => release(DEVICE_KEYBOARD, pid),
        Descriptor::Mouse => release(DEVICE_MOUSE, pid),
        Descriptor::Framebuffer(_) => release(DEVICE_FRAMEBUFFER, pid),
        _ => {}
    }
}

pub fn register_name(name: &str, pid: u32) -> bool {
    let registry = NAME_REGISTRY.get_mut();
    if registry.iter().any(|(n, _)| n == name) {
        return false;
    }
    registry.push((String::from(name), pid));
    true
}

pub fn find_pid(name: &str) -> Option<u32> {
    NAME_REGISTRY.get().iter().find(|(n, _)| n == name).map(|(_, pid)| *pid)
}
