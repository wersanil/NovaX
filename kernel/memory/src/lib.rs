//! NovaX memory subsystem.
//!
//! This crate is deliberately bootloader-agnostic: the kernel adapts
//! limine responses into [`MemoryRegion`] slices and hands them over.
//! That keeps the crate unit-testable and reusable on other platforms.

#![no_std]

pub mod frame;
pub mod paging;

pub use frame::{FrameAllocator, PhysFrame, FRAME_SIZE};
pub use paging::{PageTableRef, ADDR_MASK, NO_EXEC, PCD, PRESENT, PWT, USER, WRITABLE};

/// Higher-half direct map offset. Any physical address `p` is accessible
/// at virtual address `hhdm + p` for as long as the bootloader's HHDM
/// mapping is alive (which, in NovaX, is forever).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HhdmOffset(pub u64);

impl HhdmOffset {
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Classification of a physical region, mirroring limine's entry types
/// without a dependency on the limine crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Free RAM the allocator may hand out.
    Usable,
    /// Firmware/kernel reserved memory.
    Reserved,
    /// ACPI tables that may be reclaimed after parsing.
    AcpiReclaimable,
    /// ACPI non-volatile storage.
    AcpiNvs,
    /// Known-bad RAM.
    BadMemory,
    /// Bootloader memory that is free after boot.
    BootloaderReclaimable,
    /// Kernel image and loaded modules.
    ExecutableAndModules,
    /// Framebuffer memory.
    Framebuffer,
}

/// One physical memory range from the bootloader memory map.
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    /// Physical start address.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// What the range is used for.
    pub kind: RegionKind,
}

/// Errors returned by the memory subsystem.
#[derive(Debug)]
pub enum MemoryError {
    /// No USABLE region can hold the frame bitmap.
    NoUsableRegionLargeEnough {
        /// Frames the bitmap would need.
        needed_frames: usize,
    },
    /// The frame allocator has no free frames left.
    OutOfFrames,
    /// The leaf page-table entry is already present.
    AlreadyMapped,
    /// The leaf page-table entry is not present.
    NotMapped,
    /// Address is outside the supported range.
    InvalidAddress,
    /// 5-level paging (LA57) is enabled; unsupported.
    UnsupportedPagingMode,
}

/// Convenience alias for results from this crate.
pub type MemoryResult<T> = core::result::Result<T, MemoryError>;
