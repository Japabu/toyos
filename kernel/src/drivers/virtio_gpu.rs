use core::alloc::Layout;
use core::ptr::{copy_nonoverlapping, read_volatile};

use alloc::alloc::{alloc_zeroed, dealloc};
use alloc::boxed::Box;

use super::pci::PciDevice;
use super::virtio::{BufDir, DescSlot, Virtqueue, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use crate::mm::{PAGE_2M, align_2m};
use crate::gpu::{FLAG_HARDWARE_CURSOR, Gpu, GpuInfo};
use crate::log;
use crate::shared_memory::{self, SharedToken};
use crate::sync::Lock;

// VirtIO GPU PCI identity
const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_GPU_DEVICE: u16 = 0x1050; // 0x1040 + device_id 16

// GPU command types
const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;

// GPU response types
const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;

// Pixel formats
const FORMAT_B8G8R8A8_UNORM: u32 = 1;
const FORMAT_B8G8R8X8_UNORM: u32 = 2;

// DMA page assignments
const PAGE_CONTROLQ: usize = 0;
const PAGE_CONTROLQ_BUFS: usize = 1;
const PAGE_CURSORQ: usize = 2;
const PAGE_CURSORQ_BUFS: usize = 3;

const CURSOR_SIZE: u32 = 64;
const CURSOR_RESOURCE_ID: u32 = 3;

const REQ_OFFSET: usize = 0x000;
const RESP_OFFSET: usize = 0x800;

static DMA: Lock<Option<DmaPool>> = Lock::new(None);

fn dma_phys(page: usize) -> crate::DmaAddr {
    DMA.lock().as_ref().unwrap().page_phys(page)
}

fn dma_ptr(page: usize) -> *mut u8 {
    DMA.lock().as_ref().unwrap().page_ptr(page)
}

// ---- GPU command/response structs ----

#[repr(C)]
#[derive(Clone, Copy)]
struct CtrlHeader {
    cmd_type: u32,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    ring_idx: u8,
    padding: [u8; 3],
}

impl CtrlHeader {
    fn new(cmd_type: u32) -> Self {
        Self { cmd_type, flags: 0, fence_id: 0, ctx_id: 0, ring_idx: 0, padding: [0; 3] }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct DisplayOne {
    r: Rect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RespDisplayInfo {
    hdr: CtrlHeader,
    pmodes: [DisplayOne; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceCreate2d {
    hdr: CtrlHeader,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceUnref {
    hdr: CtrlHeader,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SetScanout {
    hdr: CtrlHeader,
    r: Rect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceFlush {
    hdr: CtrlHeader,
    r: Rect,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TransferToHost2d {
    hdr: CtrlHeader,
    r: Rect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResourceAttachBacking {
    hdr: CtrlHeader,
    resource_id: u32,
    nr_entries: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MemEntry {
    addr: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CursorPos {
    scanout_id: u32,
    x: u32,
    y: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UpdateCursor {
    hdr: CtrlHeader,
    pos: CursorPos,
    resource_id: u32,
    hot_x: u32,
    hot_y: u32,
    padding: u32,
}

// ---- Framebuffer allocation tracking ----

struct FbAlloc {
    tokens: [SharedToken; 2],
    /// Physical addresses (for device DMA / attach_backing).
    phys_addrs: [u64; 2],
    /// Virtual pointers (for kernel dealloc).
    ptrs: [*mut u8; 2],
    layout: Layout,
}

// ---- GPU Controller ----

unsafe impl Send for GpuController {}

struct GpuController {
    device: VirtioDevice,
    controlq: Virtqueue,
    cursorq: Virtqueue,
    control_slot: Option<DescSlot>,
    cursor_slot: Option<DescSlot>,
    /// Physical addresses for virtqueue descriptors (device DMA).
    req_phys: u64,
    resp_phys: u64,
    cursor_req_phys: u64,
    cursor_resp_phys: u64,
    /// Virtual pointers for kernel read/write.
    req_ptr: *mut u8,
    resp_ptr: *mut u8,
    cursor_req_ptr: *mut u8,
    width: u32,
    height: u32,
    resource: u32,
    fb: FbAlloc,
    cursor_token: SharedToken,
}

impl GpuController {
    /// Copy a command struct to the request DMA buffer and submit it.
    /// Returns the response header's type field.
    fn command_raw(&mut self, req_bytes: &[u8], resp_size: u32) -> u32 {
        unsafe {
            copy_nonoverlapping(req_bytes.as_ptr(), self.req_ptr, req_bytes.len());
        }

        let slot = self.control_slot.take().expect("GPU: no control slot");
        let returned = self.controlq.submit_and_wait(
            slot,
            &[
                (self.req_phys, req_bytes.len() as u32, BufDir::Readable),
                (self.resp_phys, resp_size, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            0, // controlq index
        );
        self.control_slot = Some(returned);

        // Read response type from header
        unsafe { read_volatile(self.resp_ptr as *const u32) }
    }

    fn command<T: Copy>(&mut self, req: &T) -> u32 {
        let bytes = unsafe {
            core::slice::from_raw_parts(req as *const T as *const u8, core::mem::size_of::<T>())
        };
        self.command_raw(bytes, core::mem::size_of::<CtrlHeader>() as u32)
    }

    fn get_display_info(&mut self) -> RespDisplayInfo {
        let hdr = CtrlHeader::new(CMD_GET_DISPLAY_INFO);
        let bytes = unsafe {
            core::slice::from_raw_parts(&hdr as *const _ as *const u8, core::mem::size_of::<CtrlHeader>())
        };
        let resp_size = core::mem::size_of::<RespDisplayInfo>() as u32;

        unsafe {
            copy_nonoverlapping(bytes.as_ptr(), self.req_ptr, bytes.len());
        }

        let slot = self.control_slot.take().expect("GPU: no control slot");
        self.control_slot = Some(self.controlq.submit_and_wait(
            slot,
            &[
                (self.req_phys, bytes.len() as u32, BufDir::Readable),
                (self.resp_phys, resp_size, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            0,
        ));

        unsafe { read_volatile(self.resp_ptr as *const RespDisplayInfo) }
    }

    fn create_resource(&mut self, id: u32, format: u32, width: u32, height: u32) {
        let cmd = ResourceCreate2d {
            hdr: CtrlHeader::new(CMD_RESOURCE_CREATE_2D),
            resource_id: id,
            format,
            width,
            height,
        };
        let resp = self.command(&cmd);
        assert!(resp == RESP_OK_NODATA, "VirtIO GPU: RESOURCE_CREATE_2D failed: {:#x}", resp);
    }

    fn destroy_resource(&mut self, id: u32) {
        let cmd = ResourceUnref {
            hdr: CtrlHeader::new(CMD_RESOURCE_UNREF),
            resource_id: id,
            padding: 0,
        };
        let resp = self.command(&cmd);
        assert!(resp == RESP_OK_NODATA, "VirtIO GPU: RESOURCE_UNREF failed: {:#x}", resp);
    }

    fn attach_backing(&mut self, id: u32, addr: u64, len: u32) {
        // This command has a variable-length payload: header + mem_entry array.
        // We write them consecutively into the request buffer.
        let cmd = ResourceAttachBacking {
            hdr: CtrlHeader::new(CMD_RESOURCE_ATTACH_BACKING),
            resource_id: id,
            nr_entries: 1,
        };
        let entry = MemEntry { addr, length: len, padding: 0 };

        let cmd_size = core::mem::size_of::<ResourceAttachBacking>();
        let entry_size = core::mem::size_of::<MemEntry>();
        unsafe {
            copy_nonoverlapping(
                &cmd as *const _ as *const u8,
                self.req_ptr,
                cmd_size,
            );
            copy_nonoverlapping(
                &entry as *const _ as *const u8,
                self.req_ptr.add(cmd_size),
                entry_size,
            );
        }

        let slot = self.control_slot.take().expect("GPU: no control slot");
        self.control_slot = Some(self.controlq.submit_and_wait(
            slot,
            &[
                (self.req_phys, (cmd_size + entry_size) as u32, BufDir::Readable),
                (self.resp_phys, core::mem::size_of::<CtrlHeader>() as u32, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            0,
        ));

        let resp = unsafe { read_volatile(self.resp_ptr as *const u32) };
        assert!(resp == RESP_OK_NODATA, "VirtIO GPU: RESOURCE_ATTACH_BACKING failed: {:#x}", resp);
    }

    fn set_scanout(&mut self, scanout: u32, resource: u32, rect: Rect) {
        let cmd = SetScanout {
            hdr: CtrlHeader::new(CMD_SET_SCANOUT),
            r: rect,
            scanout_id: scanout,
            resource_id: resource,
        };
        let resp = self.command(&cmd);
        assert!(resp == RESP_OK_NODATA, "VirtIO GPU: SET_SCANOUT failed: {:#x}", resp);
    }

    fn transfer_to_host(&mut self, resource: u32, rect: Rect, offset: u64) {
        let cmd = TransferToHost2d {
            hdr: CtrlHeader::new(CMD_TRANSFER_TO_HOST_2D),
            r: rect,
            offset,
            resource_id: resource,
            padding: 0,
        };
        let resp = self.command(&cmd);
        assert!(resp == RESP_OK_NODATA, "VirtIO GPU: TRANSFER_TO_HOST_2D failed: {:#x}", resp);
    }

    fn flush(&mut self, resource: u32, rect: Rect) {
        let cmd = ResourceFlush {
            hdr: CtrlHeader::new(CMD_RESOURCE_FLUSH),
            r: rect,
            resource_id: resource,
            padding: 0,
        };
        let resp = self.command(&cmd);
        assert!(resp == RESP_OK_NODATA, "VirtIO GPU: RESOURCE_FLUSH failed: {:#x}", resp);
    }

    fn cursor_command<T: Copy>(&mut self, req: &T) {
        let bytes = unsafe {
            core::slice::from_raw_parts(req as *const T as *const u8, core::mem::size_of::<T>())
        };
        unsafe {
            copy_nonoverlapping(bytes.as_ptr(), self.cursor_req_ptr, bytes.len());
        }
        let slot = self.cursor_slot.take().expect("GPU: no cursor slot");
        self.cursor_slot = Some(self.cursorq.submit_and_wait(
            slot,
            &[
                (self.cursor_req_phys, bytes.len() as u32, BufDir::Readable),
                (self.cursor_resp_phys, core::mem::size_of::<CtrlHeader>() as u32, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            1, // cursor queue index
        ));
    }

    fn update_cursor(&mut self, x: u32, y: u32, hot_x: u32, hot_y: u32) {
        let cmd = UpdateCursor {
            hdr: CtrlHeader::new(CMD_UPDATE_CURSOR),
            pos: CursorPos { scanout_id: 0, x, y, padding: 0 },
            resource_id: CURSOR_RESOURCE_ID,
            hot_x,
            hot_y,
            padding: 0,
        };
        self.cursor_command(&cmd);
    }

    fn move_cursor(&mut self, x: u32, y: u32) {
        let cmd = UpdateCursor {
            hdr: CtrlHeader::new(CMD_MOVE_CURSOR),
            pos: CursorPos { scanout_id: 0, x, y, padding: 0 },
            resource_id: CURSOR_RESOURCE_ID,
            hot_x: 0,
            hot_y: 0,
            padding: 0,
        };
        self.cursor_command(&cmd);
    }

    /// Allocate framebuffer backing stores and register as shared memory.
    fn alloc_framebuffer(&mut self, width: u32, height: u32) -> FbAlloc {
        let fb_size = (width * height * 4) as usize;
        let fb_aligned = align_2m(fb_size);
        let fb_layout = Layout::from_size_align(fb_aligned, PAGE_2M as usize).unwrap();
        let mut tokens = [SharedToken::from_raw(0); 2];
        let mut phys_addrs = [0u64; 2];
        let mut ptrs = [core::ptr::null_mut(); 2];
        for i in 0..2 {
            let ptr = unsafe { alloc_zeroed(fb_layout) };
            assert!(!ptr.is_null(), "VirtIO GPU: framebuffer alloc failed");
            let phys_addr = crate::mm::DirectMap::phys_of(ptr);
            ptrs[i] = ptr;
            phys_addrs[i] = phys_addr;
            tokens[i] = shared_memory::register(crate::DirectMap::new(phys_addr), fb_aligned as u64);
            log!("VirtIO GPU: buffer {} at {:?} phys={:#x} ({} bytes) token={:?}", i, ptr, phys_addrs[i], fb_size, tokens[i]);
        }
        FbAlloc { tokens, phys_addrs, ptrs, layout: fb_layout }
    }

    /// Free old framebuffer backing stores and unregister shared memory.
    fn free_framebuffer(&mut self, fb: FbAlloc) {
        for i in 0..2 {
            shared_memory::unregister(fb.tokens[i]);
            unsafe { dealloc(fb.ptrs[i], fb.layout); }
        }
    }

    fn build_gpu_info(&self) -> GpuInfo {
        GpuInfo {
            tokens: self.fb.tokens,
            cursor_token: self.cursor_token,
            width: self.width,
            height: self.height,
            stride: self.width,
            pixel_format: 1, // BGR (B8G8R8X8_UNORM)
            flags: FLAG_HARDWARE_CURSOR,
        }
    }
}

impl Gpu for GpuController {
    fn present_rect(&mut self, x: u32, y: u32, w: u32, h: u32) {
        let rect = if w == 0 || h == 0 {
            Rect { x: 0, y: 0, width: self.width, height: self.height }
        } else {
            let cx = x.min(self.width);
            let cy = y.min(self.height);
            let cw = w.min(self.width - cx);
            let ch = h.min(self.height - cy);
            if cw == 0 || ch == 0 { return; }
            Rect { x: cx, y: cy, width: cw, height: ch }
        };
        let offset = (rect.y as u64 * self.width as u64 + rect.x as u64) * 4;
        self.transfer_to_host(self.resource, rect, offset);
        self.flush(self.resource, rect);
    }

    fn set_cursor(&mut self, hot_x: u32, hot_y: u32) {
        let rect = Rect { x: 0, y: 0, width: CURSOR_SIZE, height: CURSOR_SIZE };
        self.transfer_to_host(CURSOR_RESOURCE_ID, rect, 0);
        self.update_cursor(0, 0, hot_x, hot_y);
    }

    fn move_cursor(&mut self, x: u32, y: u32) {
        GpuController::move_cursor(self, x, y);
    }

    fn set_resolution(&mut self, width: u32, height: u32) -> Result<GpuInfo, ()> {
        if width == self.width && height == self.height {
            return Ok(self.build_gpu_info());
        }

        log!("VirtIO GPU: changing resolution {}x{} -> {}x{}", self.width, self.height, width, height);

        // Allocate new framebuffer backing
        let new_fb = self.alloc_framebuffer(width, height);
        let fb_size = (width * height * 4) as usize;

        // Create new GPU resource
        let old_resource = self.resource;
        self.resource += 1;
        self.create_resource(self.resource, FORMAT_B8G8R8X8_UNORM, width, height);
        self.attach_backing(self.resource, new_fb.phys_addrs[0], fb_size as u32);

        // Switch scanout to new resource
        let rect = Rect { x: 0, y: 0, width, height };
        self.set_scanout(0, self.resource, rect);

        // Destroy old resource and free old framebuffer
        self.destroy_resource(old_resource);
        let old_fb = core::mem::replace(&mut self.fb, new_fb);
        self.free_framebuffer(old_fb);

        self.width = width;
        self.height = height;

        log!("VirtIO GPU: resolution set to {}x{}", width, height);

        Ok(self.build_gpu_info())
    }
}

/// Initialize the VirtIO GPU. Returns the driver and display info on success.
pub fn init(ecam: &crate::mm::Mmio) -> Option<(Box<dyn Gpu>, GpuInfo)> {
    let pci_dev = PciDevice::find_by_id(ecam, VIRTIO_VENDOR, VIRTIO_GPU_DEVICE)?;
    log!("VirtIO GPU: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);
    *DMA.lock() = Some(DmaPool::alloc(4));

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1);

    let mut controlq = Virtqueue::new(dma_phys(PAGE_CONTROLQ), dma_ptr(PAGE_CONTROLQ));
    let mut cursorq = Virtqueue::new(dma_phys(PAGE_CURSORQ), dma_ptr(PAGE_CURSORQ));

    device.setup_queue(0, &mut controlq);
    device.setup_queue(1, &mut cursorq);
    device.enable_queue(0);
    device.enable_queue(1);
    device.activate();

    let mut control_slots = controlq.initial_slots();
    let mut cursor_slots = cursorq.initial_slots();
    let control_slot = control_slots.pop().expect("GPU: no control slots");
    let cursor_slot = cursor_slots.pop().expect("GPU: no cursor slots");
    drop(control_slots);
    drop(cursor_slots);

    let req_phys = dma_phys(PAGE_CONTROLQ_BUFS).raw() + REQ_OFFSET as u64;
    let resp_phys = dma_phys(PAGE_CONTROLQ_BUFS).raw() + RESP_OFFSET as u64;
    let cursor_req_phys = dma_phys(PAGE_CURSORQ_BUFS).raw() + REQ_OFFSET as u64;
    let cursor_resp_phys = dma_phys(PAGE_CURSORQ_BUFS).raw() + RESP_OFFSET as u64;
    let req_ptr = unsafe { dma_ptr(PAGE_CONTROLQ_BUFS).add(REQ_OFFSET) };
    let resp_ptr = unsafe { dma_ptr(PAGE_CONTROLQ_BUFS).add(RESP_OFFSET) };
    let cursor_req_ptr = unsafe { dma_ptr(PAGE_CURSORQ_BUFS).add(REQ_OFFSET) };

    let mut gpu = GpuController {
        device,
        controlq,
        cursorq,
        control_slot: Some(control_slot),
        cursor_slot: Some(cursor_slot),
        req_phys,
        resp_phys,
        cursor_req_phys,
        cursor_resp_phys,
        req_ptr,
        resp_ptr,
        cursor_req_ptr,
        width: 0,
        height: 0,
        resource: 1,
        fb: FbAlloc {
            tokens: [SharedToken::from_raw(0); 2],
            phys_addrs: [0; 2],
            ptrs: [core::ptr::null_mut(); 2],
            layout: Layout::from_size_align(PAGE_2M as usize, PAGE_2M as usize).unwrap(),
        },
        cursor_token: SharedToken::from_raw(0),
    };

    // Query display info
    let display_info = gpu.get_display_info();
    assert!(
        display_info.hdr.cmd_type == RESP_OK_DISPLAY_INFO,
        "VirtIO GPU: GET_DISPLAY_INFO failed: {:#x}", display_info.hdr.cmd_type
    );

    let scanout = &display_info.pmodes[0];
    let width = if scanout.enabled != 0 && scanout.r.width > 0 {
        scanout.r.width
    } else {
        1024 // fallback
    };
    let height = if scanout.enabled != 0 && scanout.r.height > 0 {
        scanout.r.height
    } else {
        768
    };
    log!("VirtIO GPU: display {}x{}", width, height);

    // Allocate framebuffer backing stores (2MB-aligned)
    gpu.fb = gpu.alloc_framebuffer(width, height);
    let fb_size = (width * height * 4) as usize;

    gpu.create_resource(gpu.resource, FORMAT_B8G8R8X8_UNORM, width, height);
    gpu.attach_backing(gpu.resource, gpu.fb.phys_addrs[0], fb_size as u32);

    // Set scanout to the single resource
    let rect = Rect { x: 0, y: 0, width, height };
    gpu.set_scanout(0, gpu.resource, rect);

    // Create cursor resource (64x64, BGRA with alpha)
    let cursor_bytes = (CURSOR_SIZE * CURSOR_SIZE * 4) as usize;
    let cursor_aligned = align_2m(cursor_bytes);
    let cursor_layout = Layout::from_size_align(cursor_aligned, PAGE_2M as usize).unwrap();
    let cursor_ptr = unsafe { alloc_zeroed(cursor_layout) };
    assert!(!cursor_ptr.is_null(), "VirtIO GPU: cursor alloc failed");
    let cursor_phys = crate::mm::DirectMap::phys_of(cursor_ptr);
    gpu.cursor_token = shared_memory::register(crate::DirectMap::new(cursor_phys), cursor_aligned as u64);
    gpu.create_resource(CURSOR_RESOURCE_ID, FORMAT_B8G8R8A8_UNORM, CURSOR_SIZE, CURSOR_SIZE);
    gpu.attach_backing(CURSOR_RESOURCE_ID, cursor_phys, cursor_bytes as u32);
    log!("VirtIO GPU: cursor resource at {:?} phys={:#x} token={:?}", cursor_ptr, cursor_phys, gpu.cursor_token);

    gpu.width = width;
    gpu.height = height;

    let info = gpu.build_gpu_info();

    Some((Box::new(gpu), info))
}
