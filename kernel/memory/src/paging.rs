//! x86_64 4-level paging.
//!
//! All operations act on the **active** PML4 (the one pointed to by CR3).
//! We do not create a private PML4 yet; limine's tables already map the
//! kernel image and the HHDM, and we extend them in place.

use crate::{
    frame::{FrameAllocator, FRAME_SIZE},
    HhdmOffset, MemoryError, MemoryResult,
};

/// Page is present.
pub const PRESENT: u64 = 1 << 0;
/// Page is writable.
pub const WRITABLE: u64 = 1 << 1;
/// Page is accessible from ring 3.
pub const USER: u64 = 1 << 2;
/// Entry maps a huge (2 MiB / 1 GiB) page.
pub const HUGE: u64 = 1 << 7;
/// Page is not executable (NX).
pub const NO_EXEC: u64 = 1 << 63;
/// Physical address mask for a page-table entry.
pub const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Page-level write-through.
pub const PWT: u64 = 1 << 3;
/// Page-level cache disable.
pub const PCD: u64 = 1 << 4;

/// A handle to a 4-level page table rooted at `phys`.
///
/// Operations mutate the table in place through the HHDM; the handle does
/// not own frames and never frees intermediate tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageTableRef {
    phys: u64,
    hhdm: HhdmOffset,
}

impl PageTableRef {
    /// Read CR3 and treat its physical base as the active PML4.
    ///
    /// # Safety
    ///
    /// Caller must guarantee that CR3 actually points to a valid PML4,
    /// that 5-level paging is off (LA57 = 0), and that `hhdm` covers the
    /// physical address space of all page tables reachable from it.
    pub unsafe fn active(hhdm: HhdmOffset) -> MemoryResult<Self> {
        let cr4: u64;
        // SAFETY: privileged CR4 read; the caller guarantees ring 0.
        unsafe {
            core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nostack, preserves_flags));
        }
        if cr4 & (1 << 12) != 0 {
            return Err(MemoryError::UnsupportedPagingMode);
        }
        let cr3: u64;
        // SAFETY: privileged CR3 read; CR3 is the active PML4 by contract.
        unsafe {
            core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, preserves_flags));
        }
        Ok(Self {
            phys: cr3 & ADDR_MASK,
            hhdm,
        })
    }

    /// Physical address of the PML4 this handle refers to.
    #[inline]
    pub fn root_phys(&self) -> u64 {
        self.phys
    }

    /// Build a `PageTableRef` for an arbitrary physical PML4. Used to walk
    /// and mutate a non-current address space (e.g. a user process's PML4
    /// while another process is running).
    pub fn from_phys(phys: u64, hhdm: HhdmOffset) -> Self {
        Self {
            phys: phys & ADDR_MASK,
            hhdm,
        }
    }

    /// Map `virt` to `phys` with `flags`. Intermediate tables are created
    /// from `frames` and zeroed. Errors if the leaf PTE is already present.
    pub fn map(
        &mut self,
        virt: u64,
        phys: u64,
        flags: u64,
        frames: &mut FrameAllocator,
    ) -> MemoryResult<()> {
        let pte = self.get_or_create_pte(virt, frames)?;
        if *pte & PRESENT != 0 {
            return Err(MemoryError::AlreadyMapped);
        }
        *pte = (phys & ADDR_MASK) | (flags & !ADDR_MASK);
        self.invlpg(virt);
        Ok(())
    }

    /// Clear the leaf PTE. Errors if it was not present. Does **not** free
    /// the physical frame or intermediate tables — callers must do that.
    pub fn unmap(&mut self, virt: u64, frames: &mut FrameAllocator) -> MemoryResult<u64> {
        let pte = self.find_pte(virt, frames)?;
        let val = *pte;
        if val & PRESENT == 0 {
            return Err(MemoryError::NotMapped);
        }
        *pte = 0;
        self.invlpg(virt);
        Ok(val & ADDR_MASK)
    }

    /// Return the physical address backing `virt`, if any.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        let mut table_phys = self.phys;
        for level in 0..4 {
            let idx = (virt >> (39 - 9 * level)) & 0x1FF;
            let entry = self.read_entry(table_phys, idx);
            if entry & PRESENT == 0 {
                return None;
            }
            if level == 3 {
                return Some((entry & ADDR_MASK) | (virt & 0xFFF));
            }
            if entry & HUGE != 0 {
                // 2 MiB or 1 GiB huge page — M1 does not support unmapping
                // huge pages, but translate can still return the address.
                let shift = if level == 2 { 21 } else { 30 };
                let mask = (1u64 << shift) - 1;
                return Some((entry & ADDR_MASK & !mask) | (virt & mask));
            }
            table_phys = entry & ADDR_MASK;
        }
        None
    }

    // --- internals ----------------------------------------------------------

    fn get_or_create_pte(
        &mut self,
        virt: u64,
        frames: &mut FrameAllocator,
    ) -> MemoryResult<&'static mut u64> {
        let idx0 = (virt >> 39) & 0x1FF;
        let idx1 = (virt >> 30) & 0x1FF;
        let idx2 = (virt >> 21) & 0x1FF;
        let idx3 = (virt >> 12) & 0x1FF;

        let mut table_phys = self.phys;
        for &idx in &[idx0, idx1, idx2] {
            let entry = self.entry_mut(table_phys, idx);
            if *entry & PRESENT == 0 {
                let frame = frames.alloc().ok_or(MemoryError::OutOfFrames)?;
                let new_phys = frame.start_address();
                let dst = (self.hhdm.as_u64() + new_phys) as *mut u8;
                // WHY unsafe: HHDM covers all physical memory; `new_phys` is
                // a just-allocated frame; length is exactly one page.
                unsafe {
                    core::ptr::write_bytes(dst, 0, FRAME_SIZE as usize);
                }
                // Intermediate entries: present + writable + user.
                *entry = new_phys | PRESENT | WRITABLE | USER;
            }
            table_phys = *entry & ADDR_MASK;
        }
        Ok(self.entry_mut(table_phys, idx3))
    }

    fn find_pte(&self, virt: u64, _frames: &mut FrameAllocator) -> MemoryResult<&'static mut u64> {
        let idx0 = (virt >> 39) & 0x1FF;
        let idx1 = (virt >> 30) & 0x1FF;
        let idx2 = (virt >> 21) & 0x1FF;
        let idx3 = (virt >> 12) & 0x1FF;
        let mut table_phys = self.phys;
        for &idx in &[idx0, idx1, idx2] {
            let entry = self.read_entry(table_phys, idx);
            if entry & PRESENT == 0 {
                return Err(MemoryError::NotMapped);
            }
            table_phys = entry & ADDR_MASK;
        }
        Ok(self.entry_mut(table_phys, idx3))
    }

    #[inline]
    fn read_entry(&self, table_phys: u64, idx: u64) -> u64 {
        // WHY unsafe: `table_phys` either came from CR3 or from a present
        // entry we created; both point to a valid 4 KiB page table.
        let base = (self.hhdm.as_u64() + table_phys) as *const u64;
        unsafe { *base.add(idx as usize) }
    }

    #[inline]
    fn entry_mut(&self, table_phys: u64, idx: u64) -> &'static mut u64 {
        // WHY unsafe: same justification as `read_entry`, plus `'static`
        // because the HHDM mapping and page tables live forever.
        let base = (self.hhdm.as_u64() + table_phys) as *mut u64;
        unsafe { &mut *base.add(idx as usize) }
    }

    #[inline]
    fn invlpg(&self, virt: u64) {
        // WHY unsafe: privileged instruction, we're in ring 0.
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
        }
    }
}
