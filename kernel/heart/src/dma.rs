//! Contiguous DMA buffers for userspace drivers.
//!
//! A driver receives one buffer per address space, mapped at a fixed
//! virtual address. The physical base is returned to userspace so it can
//! program device registers (legacy virtio takes a page frame number).

use alloc::vec::Vec;

use novax_memory::{PageTableRef, PhysFrame, FRAME_SIZE, NO_EXEC, PRESENT, USER, WRITABLE};
use spin::Mutex;

use crate::sched::ThreadId;
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

#[derive(Clone, Copy)]
struct DmaAlloc {
    owner: ThreadId,
    pml4: u64,
    phys: u64,
    pages: u64,
}

static ALLOCS: Mutex<[Option<DmaAlloc>; 8]> = Mutex::new([None; 8]);

/// Allocate `len` bytes of physically contiguous, zeroed memory, map it
/// into the calling thread's address space, and write `{virt, phys}`
/// (two little-endian u64s) to `out_ptr`. Returns `(virt, phys)`.
///
/// A second call for the same process returns the existing window instead
/// of remapping it.
pub fn alloc_for_current(len: u32, out_ptr: u64) -> Result<(u64, u64), DmaError> {
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
    if !crate::syscall::user_range_ok(out_ptr, 16, true) {
        return Err(DmaError::BadOutPtr);
    }
    let owner = sched::current_id();

    if let Some(existing) = ALLOCS
        .lock()
        .iter()
        .flatten()
        .find(|a| a.owner == owner)
        .copied()
    {
        let mut result = [0u8; 16];
        result[0..8].copy_from_slice(&DMA_VIRT_BASE.to_le_bytes());
        result[8..16].copy_from_slice(&existing.phys.to_le_bytes());
        crate::syscall::copy_to_user(out_ptr, &result).map_err(|_| DmaError::BadOutPtr)?;
        return Ok((DMA_VIRT_BASE, existing.phys));
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

    if let Some(slot) = ALLOCS.lock().iter_mut().find(|a| a.is_none()) {
        *slot = Some(DmaAlloc {
            owner,
            pml4,
            phys,
            pages,
        });
    } else {
        // Table full: undo the mapping so the process does not get a
        // window the kernel cannot reclaim.
        unmap_window(pml4, pages);
        free_frames(phys, pages);
        return Err(DmaError::MapFailed);
    }

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
    Ok((DMA_VIRT_BASE, phys))
}

/// Release the DMA window owned by `owner`, unmapping it and freeing its
/// frames. Idempotent.
pub fn free_for(owner: ThreadId) {
    let record = {
        let mut allocs = ALLOCS.lock();
        let mut found = None;
        for slot in allocs.iter_mut() {
            if let Some(a) = *slot {
                if a.owner == owner {
                    found = Some(a);
                    *slot = None;
                    break;
                }
            }
        }
        found
    };
    let Some(a) = record else {
        return;
    };
    unmap_window(a.pml4, a.pages);
    free_frames(a.phys, a.pages);
    crate::arch::x86_64::flush_tlb_remote(owner, a.pml4);
    serial_println!("dma: released {} page(s) of thread {}", a.pages, owner);
}

fn unmap_window(pml4: u64, pages: u64) {
    memory_mut(|m| {
        let hhdm = m.frames.hhdm();
        let mut paging = PageTableRef::from_phys(pml4, hhdm);
        for i in 0..pages {
            let _ = paging.unmap(DMA_VIRT_BASE + i * FRAME_SIZE, &mut m.frames);
        }
    });
}

fn free_frames(phys: u64, pages: u64) {
    memory_mut(|m| {
        for i in 0..pages {
            m.frames.free(PhysFrame::containing(phys + i * FRAME_SIZE));
        }
    });
}
