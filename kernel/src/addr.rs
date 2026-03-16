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
    pub const fn new(v: u64) -> Self {
        Self(v)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Convert to kernel virtual address via the high-half direct map.
    pub const fn to_virt(self) -> VirtAddr {
        VirtAddr(self.0 + PHYS_OFFSET)
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
    pub fn from_ptr<T>(ptr: *const T) -> Self {
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
    pub const fn new(v: u64) -> Self {
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
