use alloc::boxed::Box;

use super::pci::PciDevice;
use super::virtio::{BufDir, DescSlot, Virtqueue, VirtioDevice, VIRTIO_F_VERSION_1};
use super::DmaPool;
use toyos_abi::syscall::SyscallError;
use crate::mm::{Dma, Unaligned, PAGE_2M};
use crate::gpu::{FLAG_HARDWARE_CURSOR, Gpu, GpuInfo};
use crate::log;
use crate::mm::paging::CachePolicy;
use crate::object::shm::{Pages, Region};

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_GPU_DEVICE: u16 = 0x1050; // 0x1040 + device_id 16

const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_GET_EDID: u32 = 0x010a;
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_EDID: u32 = 0x1104;

const VIRTIO_GPU_F_EDID: u64 = 1 << 1;

const FORMAT_B8G8R8A8_UNORM: u32 = 1;
const FORMAT_B8G8R8X8_UNORM: u32 = 2;

// DMA layout (byte offsets)
const OFF_CONTROLQ: usize      = 0x0000;
const OFF_CONTROLQ_BUFS: usize = 0x1000;
const OFF_CURSORQ: usize       = 0x2000;
const OFF_CURSORQ_BUFS: usize  = 0x3000;
const DMA_SIZE: usize           = 0x4000;

const CURSOR_SIZE: u32 = 64;
const CURSOR_RESOURCE_ID: u32 = 3;

const REQ_OFFSET: usize = 0x000;
const RESP_OFFSET: usize = 0x800;

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

#[repr(C)]
#[derive(Clone, Copy)]
struct GetEdid {
    hdr: CtrlHeader,
    scanout: u32,
    padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RespEdid {
    hdr: CtrlHeader,
    size: u32,
    padding: u32,
    edid: [u8; 1024],
}

/// The two scanout buffers, as the driver and as a claimant see them.
///
/// **The pages are behind an `Arc` and the driver is one holder of it.** A
/// resolution change drops this and allocates again; whatever compositor
/// mapped the old buffers keeps them until it closes its handles, and the
/// pages go back to the PMM when the last of the two lets go. That replaces a
/// forced unmap-everyone, which is the one thing a capability system may not
/// do.
struct FbAlloc {
    regions: [Region; 2],
    phys_addrs: [u64; 2],
}

struct GpuController {
    device: VirtioDevice,
    controlq: Virtqueue<'static>,
    cursorq: Virtqueue<'static>,
    control_slot: Option<DescSlot>,
    cursor_slot: Option<DescSlot>,
    /// The four command and response buffers, as [`Dma`] views rather than the
    /// raw pointer/physical-address pairs they replaced — which is what makes
    /// every field of this struct `Send` on its own and deleted
    /// `unsafe impl Send for GpuController {}`.
    ///
    /// **The request buffers take the unaligned discipline and the response
    /// buffers the volatile one**, and that is the difference between writing a
    /// command and reading an answer. A command is written between one
    /// `submit_and_wait` and the next, so no descriptor names the buffer while
    /// the write lands and the bytes are a specification's layout rather than an
    /// ABI's. A response is memory the device filled, and volatile is what says
    /// the bytes are not this CPU's to cache.
    ///
    /// `'static` because the pool is leaked at `init`: the compositor's display
    /// is never unbound, and the `static Lock<Option<DmaPool>>` this replaces was
    /// never cleared either — it just did not say so.
    req: Dma<'static, Unaligned>,
    resp: Dma<'static>,
    cursor_req: Dma<'static, Unaligned>,
    #[allow(dead_code)] // the cursor queue's answers are not read
    cursor_resp: Dma<'static>,
    width: u32,
    height: u32,
    resource: u32,
    fb: FbAlloc,
    cursor: Region,
}

impl GpuController {
    /// What the device wrote into the control response buffer.
    ///
    /// **The one reader of DMA memory in this driver**, and the reason `resp`
    /// carries the volatile discipline: the device wrote these bytes, so the
    /// load is not this CPU's to cache, and it is bounded for the whole `T`
    /// where `ptr_at` would only have bounded the offset.
    fn answer<T: Copy>(&self) -> T {
        // Bounded for all of `T` against a 0x800-byte buffer, and the transfer
        // that filled it has completed: `submit_and_wait` returned, and
        // `poll_used`'s `fence(Acquire)` orders the device's writes before this
        // read.
        self.resp.read(0)
    }

    /// Put one command struct in the request buffer, submit it, and answer with
    /// whatever the device wrote back.
    ///
    /// Typed rather than `&[u8]`: `command_raw` took a byte slice, so every
    /// caller built one out of its command with `from_raw_parts` — three more
    /// unsafe blocks for a view nothing else used. Writing the `T` itself
    /// writes the same bytes.
    fn command_of<Req: Copy, Resp: Copy>(&mut self, req: &Req) -> Resp {
        self.req.write(0, *req);

        let slot = self.control_slot.take().expect("GPU: no control slot");
        let returned = self.controlq.submit_and_wait(
            slot,
            &[
                (self.req.phys(), core::mem::size_of::<Req>() as u32, BufDir::Readable),
                (self.resp.phys(), core::mem::size_of::<Resp>() as u32, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            0, // controlq index
        );
        self.control_slot = Some(returned);

        self.answer()
    }

    /// A command whose answer is only its response header's type field, which
    /// is every command but GET_EDID.
    fn command<T: Copy>(&mut self, req: &T) -> u32 {
        // `CtrlHeader` and not `u32` as the response size: the device is told
        // how much room it has, and a header is what it writes.
        let hdr: CtrlHeader = self.command_of(req);
        hdr.cmd_type
    }

    fn get_edid(&mut self, scanout: u32) -> RespEdid {
        let cmd = GetEdid {
            hdr: CtrlHeader::new(CMD_GET_EDID),
            scanout,
            padding: 0,
        };
        self.command_of(&cmd)
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
        self.req.write(0, cmd);
        self.req.write(cmd_size, entry);

        let slot = self.control_slot.take().expect("GPU: no control slot");
        self.control_slot = Some(self.controlq.submit_and_wait(
            slot,
            &[
                (self.req.phys(), (cmd_size + entry_size) as u32, BufDir::Readable),
                (self.resp.phys(), core::mem::size_of::<CtrlHeader>() as u32, BufDir::Writable),
            ],
            self.device.notify_mmio(),
            self.device.notify_off_multiplier(),
            0,
        ));

        let resp = self.answer::<CtrlHeader>().cmd_type;
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
        self.cursor_req.write(0, *req);
        let slot = self.cursor_slot.take().expect("GPU: no cursor slot");
        self.cursor_slot = Some(self.cursorq.submit_and_wait(
            slot,
            &[
                (self.cursor_req.phys(), core::mem::size_of::<T>() as u32, BufDir::Readable),
                (self.cursor_resp.phys(), core::mem::size_of::<CtrlHeader>() as u32, BufDir::Writable),
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
    /// `None` when the physical memory is not there — the caller decides
    /// whether that is fatal.
    fn alloc_framebuffer(&mut self, fb_size: u32) -> Option<FbAlloc> {
        let fb_size = fb_size as usize;
        let fb_pages = fb_size.div_ceil(PAGE_2M as usize);
        let fb_aligned = (fb_pages * PAGE_2M as usize) as u64;
        let mut phys_addrs = [0u64; 2];
        let all_pages =
            [crate::mm::pmm::alloc_contiguous(fb_pages, crate::mm::pmm::Category::Framebuffer)?,
             crate::mm::pmm::alloc_contiguous(fb_pages, crate::mm::pmm::Category::Framebuffer)?];
        let regions = all_pages.map(|pages| {
            let phys = pages[0].direct_map().phys();
            Region {
                phys: crate::DirectMap::from_phys(phys),
                size: fb_aligned,
                cache: CachePolicy::DeferToMtrr,
                pages: Some(alloc::sync::Arc::new(Pages::new(pages))),
            }
        });
        for i in 0..2 {
            phys_addrs[i] = regions[i].phys.phys();
            log!(
                "VirtIO GPU: buffer {} at {:?} phys={:#x} ({} bytes)",
                i, regions[i].phys.as_mut_ptr::<u8>(), phys_addrs[i], fb_size
            );
        }
        Some(FbAlloc { regions, phys_addrs })
    }

    fn build_gpu_info(&self) -> GpuInfo {
        GpuInfo {
            scanout: self.fb.regions.clone(),
            cursor: self.cursor.clone(),
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

    fn set_resolution(&mut self, width: u32, height: u32) -> Result<GpuInfo, SyscallError> {
        if width == self.width && height == self.height {
            return Ok(self.build_gpu_info());
        }

        let fb_size = fb_size_bytes(width, height).ok_or(SyscallError::InvalidArgument)?;

        log!("VirtIO GPU: changing resolution {}x{} -> {}x{}", self.width, self.height, width, height);

        // Allocate new framebuffer backing. The old pair stays live until the
        // swap below, so a refusal here leaves the display exactly as it was.
        let new_fb = self.alloc_framebuffer(fb_size).ok_or(SyscallError::ResourceExhausted)?;

        let old_resource = self.resource;
        self.resource += 1;
        self.create_resource(self.resource, FORMAT_B8G8R8X8_UNORM, width, height);
        self.attach_backing(self.resource, new_fb.phys_addrs[0], fb_size);

        let rect = Rect { x: 0, y: 0, width, height };
        self.set_scanout(0, self.resource, rect);

        self.destroy_resource(old_resource);
        // The old pages go when the last holder lets go, which may be a
        // compositor that has not yet mapped the new ones. Nothing is revoked.
        drop(core::mem::replace(&mut self.fb, new_fb));

        self.width = width;
        self.height = height;

        log!("VirtIO GPU: resolution set to {}x{}", width, height);

        Ok(self.build_gpu_info())
    }
}

/// Framebuffer bytes for a resolution, or `None` if the device could never
/// back it: zero-sized, or past the u32 length `attach_backing` carries.
/// Both dimensions are userland-chosen via `SYS_GPU_SET_RESOLUTION`, so the
/// product is computed in u64 — `width * height * 4` in u32 wraps.
fn fb_size_bytes(width: u32, height: u32) -> Option<u32> {
    let bytes = (width as u64).checked_mul(height as u64)?.checked_mul(4)?;
    if bytes == 0 || bytes > u32::MAX as u64 {
        return None;
    }
    Some(bytes as u32)
}

/// Initialize the VirtIO GPU. Returns the driver and display info on success.
pub fn init(devices: &[PciDevice]) -> Option<(Box<dyn Gpu>, GpuInfo)> {
    let pci_dev = *devices.iter().find(|d| d.is_id(VIRTIO_VENDOR, VIRTIO_GPU_DEVICE))?;
    log!("VirtIO GPU: found at PCI {:02x}:{:02x}.{}", pci_dev.bus, pci_dev.dev, pci_dev.func);
    // Leaked rather than held in a `static`: the display is never unbound.
    let dma = DmaPool::alloc(DMA_SIZE).leak();

    let device = VirtioDevice::init(&pci_dev, VIRTIO_F_VERSION_1 | VIRTIO_GPU_F_EDID);

    let mut controlq = Virtqueue::new(dma.subview(OFF_CONTROLQ, 0x1000), 16);
    let mut cursorq = Virtqueue::new(dma.subview(OFF_CURSORQ, 0x1000), 16);

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

    let ctrl_bufs = dma.subview(OFF_CONTROLQ_BUFS, 0x1000);
    let cursor_bufs = dma.subview(OFF_CURSORQ_BUFS, 0x1000);
    // Each half of a buffer page: request at 0, response at `RESP_OFFSET`, and
    // the length is what every write and read is bounded against.
    const HALF: usize = RESP_OFFSET - REQ_OFFSET;

    let mut gpu = GpuController {
        device,
        controlq,
        cursorq,
        control_slot: Some(control_slot),
        cursor_slot: Some(cursor_slot),
        req: ctrl_bufs.subview(REQ_OFFSET, HALF).unaligned(),
        resp: ctrl_bufs.subview(RESP_OFFSET, HALF),
        cursor_req: cursor_bufs.subview(REQ_OFFSET, HALF).unaligned(),
        cursor_resp: cursor_bufs.subview(RESP_OFFSET, HALF),
        width: 0,
        height: 0,
        resource: 1,
        fb: FbAlloc {
            regions: core::array::from_fn(|_| Region::empty()),
            phys_addrs: [0; 2],
        },
        cursor: Region::empty(),
    };

    // EDID reports firmware-set resolution (often 640x480 from OVMF), not the
    // host-configured preferred resolution. Query EDID for the preferred mode
    // from the first Detailed Timing Descriptor.
    let edid = gpu.get_edid(0);
    let (width, height) = if edid.hdr.cmd_type == RESP_OK_EDID {
        let dtd = &edid.edid[54..72];
        let w = dtd[2] as u32 | ((dtd[4] as u32 >> 4) << 8);
        let h = dtd[5] as u32 | ((dtd[7] as u32 >> 4) << 8);
        if w >= 1280 && h >= 720 {
            (w, h)
        } else {
            (1280, 720) // EDID reports stale firmware resolution, use default
        }
    } else {
        (1280, 720)
    };
    log!("VirtIO GPU: display {}x{}", width, height);

    // Allocate framebuffer backing stores (2MB-aligned). Boot-time, and the
    // dimensions come from EDID or the default above — a failure here is a
    // machine that cannot run, so it dies loudly.
    let fb_size = fb_size_bytes(width, height).expect("VirtIO GPU: nonsense display dimensions");
    gpu.fb = gpu.alloc_framebuffer(fb_size).expect("VirtIO GPU: framebuffer alloc failed");

    gpu.create_resource(gpu.resource, FORMAT_B8G8R8X8_UNORM, width, height);
    gpu.attach_backing(gpu.resource, gpu.fb.phys_addrs[0], fb_size);

    let rect = Rect { x: 0, y: 0, width, height };
    gpu.set_scanout(0, gpu.resource, rect);

    // Create cursor resource (64x64, BGRA with alpha)
    let cursor_bytes = (CURSOR_SIZE * CURSOR_SIZE * 4) as usize;
    let cursor_pages = crate::mm::pmm::alloc_contiguous(1, crate::mm::pmm::Category::Framebuffer).expect("VirtIO GPU: cursor alloc failed");
    let cursor_ptr = cursor_pages[0].direct_map().as_mut_ptr::<u8>();
    let cursor_phys = cursor_pages[0].direct_map().phys();
    gpu.cursor = Region {
        phys: crate::DirectMap::from_phys(cursor_phys),
        size: PAGE_2M,
        cache: CachePolicy::DeferToMtrr,
        pages: Some(alloc::sync::Arc::new(Pages::new(cursor_pages))),
    };
    gpu.create_resource(CURSOR_RESOURCE_ID, FORMAT_B8G8R8A8_UNORM, CURSOR_SIZE, CURSOR_SIZE);
    gpu.attach_backing(CURSOR_RESOURCE_ID, cursor_phys, cursor_bytes as u32);
    log!("VirtIO GPU: cursor resource at {:?} phys={:#x}", cursor_ptr, cursor_phys);

    gpu.width = width;
    gpu.height = height;

    let info = gpu.build_gpu_info();

    Some((Box::new(gpu), info))
}
