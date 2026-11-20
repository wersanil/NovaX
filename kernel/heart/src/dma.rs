//! Contiguous DMA buffers for userspace drivers.
//!
//! A driver receives one buffer per address space, mapped at a fixed
//! virtual address. The physical base is returned to userspace so it can
//! program device registers (legacy virtio takes a page frame number).

use alloc::vec::Vec;

use novax_memory::{PageTableRef, PhysFrame, FRAME_SIZE, NO_EXEC, PRESENT, USER, WRITABLE};

use crate::{memory_mut, sched, serial_println};

/// Fixed user virtual address of the DMA window. It sits between the
/// program image (0x400000) and the user stack (0x7fff_ffff_0000).
pub const DMA_VIRT_BASE: u64 = 0x0000_3000_0000;

/// Why a DMA allocation or mapping failed.
#[derive(Debug)]
pub enum DmaError {
    /// Kernel threads have no address space to map into.
    NotUserThread,
    /// Zero-length or absurdly large request.
    BadLength,
    /// The frame allocator ran out of memory.
    OutOfFrames,
    /// A page in the DMA window was already mapped.
    MapFailed,
    /// The result structure could not be written to user memory.
    BadOutPtr,
}

/// Allocate `len` bytes of physically contiguous, zeroed memory, map it
/// into the calling thread's address space, and write `{virt, phys}`
/// (two little-endian u64s) to `out_ptr`.
pub fn alloc_for_current(len: u32, out_ptr: u64) -> Result<(), DmaError> {
    if len == 0 {
        return Err(DmaError::BadLength);
    }
    let pages = (len as u64).div_ceil(FRAME_SIZE);
    if pages > 1024 {
        return Err(DmaError::BadLength);
    }

    let pml4 = sched::current_pml4_phys();
    if pml4 == 0 {
        return Err(DmaError::NotUserThread);
    }

    let phys = memory_mut(|m| {
        let hhdm = m.frames.hhdm();
        let mut paging = PageTableRef::from_phys(pml4, hhdm);

        let mut frames: Vec<PhysFrame> = Vec::with_capacity(pages as usize);
        for _ in 0..pages {
            frames.push(m.frames.alloc().ok_or(DmaError::OutOfFrames)?);
        }

        for i in 0..frames.len() {
            let frame = frames[i];
            let va = DMA_VIRT_BASE + i as u64 * FRAME_SIZE;
            if paging
                .map(
                    va,
                    frame.start_address(),
                    PRESENT | USER | WRITABLE | NO_EXEC,
                    &mut m.frames,
                )
                .is_err()
            {
                // Roll back the pages mapped so far; the address space is
                // left exactly as we found it. Unmap cannot fail for pages
                // we mapped in this loop, so the Result is best-effort.
                for j in 0..i {
                    let _ = paging.unmap(DMA_VIRT_BASE + j as u64 * FRAME_SIZE, &mut m.frames);
                }
                for f in &frames {
                    m.frames.free(*f);
                }
                return Err(DmaError::MapFailed);
            }

            let dst = (hhdm.as_u64() + frame.start_address()) as *mut u8;
            // WHY unsafe: `dst` points into a frame we just mapped and own;
            // the length is exactly one page.
            unsafe {
                core::ptr::write_bytes(dst, 0, FRAME_SIZE as usize);
            }
        }

        Ok::<u64, DmaError>(frames[0].start_address())
    })?;

    let mut result = [0u8; 16];
    result[0..8].copy_from_slice(&DMA_VIRT_BASE.to_le_bytes());
    result[8..16].copy_from_slice(&phys.to_le_bytes());
    crate::syscall::copy_to_user(out_ptr, &result).map_err(|_| DmaError::BadOutPtr)?;

    serial_println!(
        "dma: {} page(s) at {:#x} (phys {:#x})",
        pages,
        DMA_VIRT_BASE,
        phys
    );
    Ok(())
}
