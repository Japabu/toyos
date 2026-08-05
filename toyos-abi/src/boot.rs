#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KernelArgs {
    pub memory_map_addr: u64,
    pub memory_map_size: u64,
    pub kernel_memory_addr: u64,
    pub kernel_memory_size: u64,
    pub kernel_stack_addr: u64,
    pub kernel_stack_size: u64,
    pub rsdp_addr: u64,
    pub initrd_addr: u64,
    pub initrd_size: u64,
    pub init_program_addr: u64,
    pub init_program_len: u64,
    pub kernel_elf_addr: u64,
    pub kernel_elf_size: u64,
    pub gop_framebuffer: u64,
    pub gop_framebuffer_size: u64,
    pub gop_width: u32,
    pub gop_height: u32,
    pub gop_stride: u32,
    pub gop_pixel_format: u32,
    /// Physical address of the bootloader's page table (has both identity map and high-half).
    /// Used by the SMP trampoline for AP boot transition.
    pub boot_pml4_addr: u64,
    /// First logical block of the partition the firmware loaded this image
    /// from, in that device's own block size.
    pub boot_partition_start_lba: u64,
    /// That partition's length, in the same blocks.
    ///
    /// Firmware's number, kept alongside the GUID so the kernel has two
    /// independent accounts of the partition's extent — this one and the GPT
    /// entry it finds. A disagreement means the table on the disk is not the
    /// table firmware read, and the kernel refuses rather than picking one.
    pub boot_partition_blocks: u64,
    /// The partition's *unique* GUID, exactly as it sits in the HARDDRIVE
    /// device path node and in the GPT entry — no byte order conversion on
    /// either side, so the comparison that decides which partition is ours
    /// cannot be got backwards.
    pub boot_partition_guid: [u8; 16],
    /// Zero when this machine has no designated boot partition, in which case
    /// the three fields above are zero as well.
    ///
    /// Not an error: booting over the network, or off a device with no
    /// partition table, is a machine ToyOS is expected to come up on. The
    /// kernel simply knows it has no partition it is entitled to write to.
    pub boot_partition_present: u32,
    /// The unique GUID of the partition the kernel's log goes on, read out of
    /// `\toyos\log.guid` on the volume the bootloader loaded itself from, in
    /// the same raw byte order as [`Self::boot_partition_guid`].
    ///
    /// No presence flag, unlike the boot partition above, and not because the
    /// state cannot arise but because it is not a machine. A machine really can
    /// have no boot partition to be named — PXE, an unpartitioned disk. But
    /// this GUID comes from a file `create_fat_volume` writes beside
    /// `kernel.elf` and `initrd.img`, so a volume carrying those two and not
    /// this one was not built by this project, and the bootloader refuses it by
    /// name rather than starting a kernel that would silently have nowhere to
    /// put its log.
    ///
    /// Naming the partition is all this does. Whether one with that GUID is on
    /// the disk is the kernel's question, and its answer there may well be no.
    pub log_partition_guid: [u8; 16],
    /// Minutes to add to the CMOS RTC's own reading to get UTC, as firmware
    /// reported it in `EFI_TIME::TimeZone`.
    ///
    /// The RTC's registers carry a wall clock and no zone, and no two operating
    /// systems agree on which zone that is: a machine that has ever run Windows
    /// keeps local time there, one that has only run Linux keeps UTC. Firmware
    /// is the one party that both knows and can be asked, and `GetTime` is the
    /// call — a *runtime* service, so it is asked here rather than in the
    /// kernel, which never maps the runtime.
    ///
    /// UEFI's relation is `Localtime = UTC - TimeZone`, so a machine keeping
    /// local time in UTC+2 reports -120 and the kernel adds -120 minutes to what
    /// it reads off the CMOS.
    pub rtc_utc_offset_minutes: i32,
    /// Whether firmware answered the question above at all.
    ///
    /// Zero when `GetTime` failed, or reported `EFI_UNSPECIFIED_TIMEZONE`, or
    /// named an offset outside the range its own spec gives the field. The
    /// middle one is the ordinary state of a machine nothing has ever told its
    /// zone to, and it is what OVMF ships. The kernel then treats the RTC as UTC
    /// and says so, because with the one party that knows declining to answer
    /// there is nothing else left to assume.
    ///
    /// A flag rather than a sentinel in the field above, for the same reason
    /// [`Self::boot_partition_present`] is one: `0x7FF` is a value the *wire*
    /// format defines, and carrying it inward would make every reader of this
    /// struct know that.
    pub rtc_utc_offset_known: u32,
}

impl KernelArgs {
    /// Firmware's answer about the zone the RTC keeps, as one value.
    ///
    /// The two fields exist because this struct is a C layout shared by two
    /// binaries; this is where they become the option they describe, and no
    /// caller inward of here handles the pair.
    pub fn rtc_utc_offset(&self) -> Option<i32> {
        (self.rtc_utc_offset_known != 0).then_some(self.rtc_utc_offset_minutes)
    }
}

/// The kernel's `_start` reads three of these fields out of `rdi` by hardcoded
/// byte offset, before Rust code runs and before there is a stack to call a
/// getter on. Adding a field anywhere but the end moves them silently, and the
/// symptom is a stack pointer pointing at nothing.
///
/// The size and alignment are here for the other half of the contract: the
/// bootloader writes this struct and the kernel reads it, and the two are
/// separate binaries built for separate targets. They share this file, so they
/// cannot disagree about the layout — but only as long as nothing else does
/// the arithmetic by hand.
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(offset_of!(KernelArgs, kernel_memory_addr) == 16);
    assert!(offset_of!(KernelArgs, kernel_stack_addr) == 32);
    assert!(offset_of!(KernelArgs, kernel_stack_size) == 40);
    assert!(offset_of!(KernelArgs, boot_partition_start_lba) == 144);
    assert!(offset_of!(KernelArgs, boot_partition_blocks) == 152);
    assert!(offset_of!(KernelArgs, boot_partition_guid) == 160);
    assert!(offset_of!(KernelArgs, boot_partition_present) == 176);
    assert!(offset_of!(KernelArgs, log_partition_guid) == 180);
    assert!(offset_of!(KernelArgs, rtc_utc_offset_minutes) == 196);
    assert!(offset_of!(KernelArgs, rtc_utc_offset_known) == 200);
    assert!(size_of::<KernelArgs>() == 208);
    assert!(align_of::<KernelArgs>() == 8);
};

#[repr(C)]
#[derive(Debug)]
pub struct MemoryMapEntry {
    pub uefi_type: u32,
    pub start: u64,
    pub end: u64,
}