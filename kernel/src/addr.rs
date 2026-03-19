use core::fmt;
use core::ops::{Add, Sub};

/// All physical memory is mapped at this virtual offset in the kernel's address space.
/// Physical address P is accessible at virtual address P + PHYS_OFFSET.
pub const PHYS_OFFSET: u64 = 0xFFFF_8000_0000_0000;

/// Physical memory address. Dereferenceable via `as_ptr()` which adds PHYS_OFFSET.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PhysAddr(u64);

/// Kernel virtual address (in the high-half direct map). Can be dereferenced via `as_ptr`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct VirtAddr(u64);

/// User-space virtual address. Lives in a separate address space and is **not**
/// directly dereferenceable. Must go through page table translation or
/// `SyscallContext` validation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct UserAddr(u64);

// ---------------------------------------------------------------------------
// PhysAddr
// ---------------------------------------------------------------------------

impl PhysAddr {
    /// Create from a raw physical address. Use `new` only in const contexts;
    /// prefer runtime code paths where `from_ptr` or `checked` catch mistakes.
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    /// Runtime-checked constructor. Panics in debug builds if the value looks
    /// like a kernel virtual address (>= PHYS_OFFSET).
    pub fn checked(v: u64) -> Self {
        debug_assert!(v < PHYS_OFFSET, "PhysAddr::checked({:#x}): looks like a virtual address", v);
        Self(v)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Convert to kernel virtual address via the high-half direct map.
    pub const fn to_virt(self) -> VirtAddr {
        VirtAddr(self.0 + PHYS_OFFSET)
    }

    /// Convert to a KernelAddr for structured memory access.
    pub const fn to_kernel(self) -> KernelAddr {
        KernelAddr((self.0 + PHYS_OFFSET) as *const u8)
    }

    /// Dereferenceable pointer via the high-half direct map.
    pub const fn as_ptr<T>(self) -> *const T {
        (self.0 + PHYS_OFFSET) as *const T
    }

    /// Dereferenceable mutable pointer via the high-half direct map.
    pub const fn as_mut_ptr<T>(self) -> *mut T {
        (self.0 + PHYS_OFFSET) as *mut T
    }

    /// Convert a kernel virtual pointer back to a physical address.
    /// The pointer must be in the high-half direct map (>= PHYS_OFFSET).
    pub fn from_ptr<T>(ptr: *const T) -> Self {
        debug_assert!(
            ptr as u64 >= PHYS_OFFSET,
            "PhysAddr::from_ptr: {:#x} is not a direct-map address",
            ptr as u64,
        );
        Self(ptr as u64 - PHYS_OFFSET)
    }

    /// Page frame number (address / 4096).
    pub const fn pfn(self) -> u64 {
        self.0 >> 12
    }

    pub const fn from_pfn(pfn: u64) -> Self {
        Self(pfn << 12)
    }

    pub const fn align_up(self, alignment: u64) -> Self {
        Self((self.0 + alignment - 1) & !(alignment - 1))
    }

    pub const fn align_down(self, alignment: u64) -> Self {
        Self(self.0 & !(alignment - 1))
    }

    pub const fn is_aligned(self, alignment: u64) -> bool {
        self.0 & (alignment - 1) == 0
    }

    pub const fn page_offset_4k(self) -> u64 {
        self.0 & 0xFFF
    }

    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

impl Add<u64> for PhysAddr {
    type Output = Self;
    fn add(self, rhs: u64) -> Self {
        Self(self.0 + rhs)
    }
}

impl Sub<u64> for PhysAddr {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self {
        Self(self.0 - rhs)
    }
}

impl Sub for PhysAddr {
    type Output = u64;
    fn sub(self, rhs: Self) -> u64 {
        self.0 - rhs.0
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#018x})", self.0)
    }
}

impl fmt::Display for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl fmt::LowerHex for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// VirtAddr
// ---------------------------------------------------------------------------

impl VirtAddr {
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Convert high-half virtual address back to physical.
    pub const fn to_phys(self) -> PhysAddr {
        PhysAddr(self.0 - PHYS_OFFSET)
    }

    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }

    pub fn from_ptr<T>(ptr: *const T) -> Self {
        Self(ptr as u64)
    }

    pub const fn align_up(self, alignment: u64) -> Self {
        Self((self.0 + alignment - 1) & !(alignment - 1))
    }

    pub const fn align_down(self, alignment: u64) -> Self {
        Self(self.0 & !(alignment - 1))
    }

    pub const fn is_aligned(self, alignment: u64) -> bool {
        self.0 & (alignment - 1) == 0
    }

    pub const fn page_offset_4k(self) -> u64 {
        self.0 & 0xFFF
    }

    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

impl Add<u64> for VirtAddr {
    type Output = Self;
    fn add(self, rhs: u64) -> Self {
        Self(self.0 + rhs)
    }
}

impl Sub<u64> for VirtAddr {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self {
        Self(self.0 - rhs)
    }
}

impl Sub for VirtAddr {
    type Output = u64;
    fn sub(self, rhs: Self) -> u64 {
        self.0 - rhs.0
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#018x})", self.0)
    }
}

impl fmt::Display for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl fmt::LowerHex for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// UserAddr
// ---------------------------------------------------------------------------

impl UserAddr {
    /// Only VmaList, address space constants, and kernel internals create these.
    /// Syscall handlers and driver code use VmaList::alloc_region instead.
    pub(crate) const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Whether this address is in the canonical user half (below the hole).
    pub const fn is_canonical(self) -> bool {
        self.0 < 0x0000_8000_0000_0000
    }

    pub const fn align_up(self, alignment: u64) -> Self {
        Self((self.0 + alignment - 1) & !(alignment - 1))
    }

    pub const fn align_down(self, alignment: u64) -> Self {
        Self(self.0 & !(alignment - 1))
    }

    pub const fn is_aligned(self, alignment: u64) -> bool {
        self.0 & (alignment - 1) == 0
    }

    pub const fn page_offset_4k(self) -> u64 {
        self.0 & 0xFFF
    }
}

impl Add<u64> for UserAddr {
    type Output = Self;
    fn add(self, rhs: u64) -> Self {
        Self(self.0 + rhs)
    }
}

impl Sub<u64> for UserAddr {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self {
        Self(self.0 - rhs)
    }
}

impl Sub for UserAddr {
    type Output = u64;
    fn sub(self, rhs: Self) -> u64 {
        self.0 - rhs.0
    }
}

impl fmt::Debug for UserAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UserAddr({:#018x})", self.0)
    }
}

impl fmt::Display for UserAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0)
    }
}

impl fmt::LowerHex for UserAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
    }
}

// ---------------------------------------------------------------------------
// KernelAddr
// ---------------------------------------------------------------------------

/// Kernel virtual pointer into the high-half direct map. Wraps a raw pointer
/// with arithmetic helpers for ELF parsing and other structured memory access.
/// Created from `PhysAddr::to_kernel()` — cannot be constructed from user addresses.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct KernelAddr(*const u8);

// SAFETY: KernelAddr points into the kernel direct map which is globally accessible.
unsafe impl Send for KernelAddr {}
unsafe impl Sync for KernelAddr {}

impl KernelAddr {
    pub const fn null() -> Self {
        Self(core::ptr::null())
    }

    pub const fn is_null(self) -> bool {
        self.0.is_null()
    }

    pub fn raw(self) -> u64 {
        self.0 as u64
    }

    pub const fn as_ptr<T>(self) -> *const T {
        self.0 as *const T
    }

    pub const fn as_mut_ptr<T>(self) -> *mut T {
        self.0 as *mut T
    }

    /// Read a value at byte offset from this address.
    pub unsafe fn read_at<T: Copy>(self, byte_offset: usize) -> T {
        (self.0.add(byte_offset) as *const T).read_unaligned()
    }

    /// Offset by bytes, returning a new KernelAddr.
    pub const fn add(self, bytes: usize) -> Self {
        Self(unsafe { self.0.add(bytes) })
    }

    /// Convert back to physical address.
    pub fn to_phys(self) -> PhysAddr {
        PhysAddr::from_ptr(self.0)
    }

    /// Create from a raw kernel virtual pointer.
    pub fn from_ptr<T>(ptr: *const T) -> Self {
        debug_assert!(
            ptr as u64 >= PHYS_OFFSET || ptr.is_null(),
            "KernelAddr::from_ptr: {:#x} is not a direct-map address",
            ptr as u64,
        );
        Self(ptr as *const u8)
    }
}

impl Add<u64> for KernelAddr {
    type Output = Self;
    fn add(self, rhs: u64) -> Self {
        Self(unsafe { self.0.add(rhs as usize) })
    }
}

impl Sub<u64> for KernelAddr {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self {
        Self(unsafe { self.0.sub(rhs as usize) })
    }
}

impl Sub<KernelAddr> for KernelAddr {
    type Output = u64;
    fn sub(self, rhs: Self) -> u64 {
        self.0 as u64 - rhs.0 as u64
    }
}

impl fmt::Debug for KernelAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KernelAddr({:#018x})", self.0 as u64)
    }
}

impl fmt::Display for KernelAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#018x}", self.0 as u64)
    }
}

impl fmt::LowerHex for KernelAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&(self.0 as u64), f)
    }
}

// ---------------------------------------------------------------------------
// DmaAddr
// ---------------------------------------------------------------------------

/// Physical address for DMA device access. Cannot be accidentally created from
/// a virtual pointer — only from `PhysAddr` (which validates the source).
/// Use `.raw()` when writing to hardware descriptor fields.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct DmaAddr(u64);

impl DmaAddr {
    /// The raw physical address for hardware descriptor fields.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl From<PhysAddr> for DmaAddr {
    fn from(addr: PhysAddr) -> Self {
        Self(addr.raw())
    }
}

impl fmt::Debug for DmaAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DmaAddr({:#018x})", self.0)
    }
}
