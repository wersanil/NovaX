//! Capability-checked physical MMIO mapping.
//!
//! The kernel never touches device registers or framebuffer pixels itself.
//! It only maps a physical window into the address space of the thread that
//! owns a matching [`crate::ipc::CapKind::Mmio`] capability.

use novax_memory::{PageTableRef, FRAME_SIZE, NO_EXEC, PCD, PRESENT, PWT, USER, WRITABLE};

use crate::{memory_mut, sched, serial_println};

/// Fixed user virtual address for MMIO windows. Sits between the program
/// image (0x40_0000), the DMA window (0x3000_0000) and the user stack.
pub const MMIO_VIRT_BASE: u64 = 0x0000_4000_0000;

/// Upper bound on a single MMIO capability (8K RGBA framebuffer is 256 MiB).
const MAX_MMIO_LEN: u64 = 256 * 1024 * 1024;

/// Why an MMIO mapping failed.
#[derive(Debug)]
pub enum MmioError {
    /// Kernel threads have no address space to map into.
    NotUserThread,
    /// Zero-length, overflowing or absurdly large window.
    BadLength,
    /// A page in the MMIO window was already mapped.
    MapFailed,
    /// The result structure could not be written to user memory.
    BadOutPtr,
}

/// Map `len` bytes starting at physical `paddr` into the calling thread.
///
/// On success writes `{virt, paddr, len}` (three little-endian `u64`s) to
/// `out_ptr`. The mapping is uncached (`PCD | PWT`) and non-executable.
pub fn map_for_current(paddr: u64, len: u64, out_ptr: u64) -> Result<(), MmioError> {
    if len == 0 || len > MAX_MMIO_LEN || paddr == 0 {
        return Err(MmioError::BadLength);
    }

    let offset = paddr & (FRAME_SIZE - 1);
    let base = paddr - offset;
    let map_len = len.checked_add(offset).ok_or(MmioError::BadLength)?;
    if map_len > MAX_MMIO_LEN + FRAME_SIZE {
        return Err(MmioError::BadLength);
    }
    let _end = base.checked_add(map_len).ok_or(MmioError::BadLength)?;
    let pages = map_len.div_ceil(FRAME_SIZE);

    let pml4 = sched::current_pml4_phys();
    if pml4 == 0 {
        return Err(MmioError::NotUserThread);
    }
    // Validate the result pointer before mapping the window.
    if !crate::syscall::user_range_ok(out_ptr, 24, true) {
        return Err(MmioError::BadOutPtr);
    }

    memory_mut(|m| {
        let hhdm = m.frames.hhdm();
        let mut paging = PageTableRef::from_phys(pml4, hhdm);
        let flags = PRESENT | USER | WRITABLE | NO_EXEC | PCD | PWT;

        for i in 0..pages {
            let va = MMIO_VIRT_BASE + i * FRAME_SIZE;
            let pa = base + i * FRAME_SIZE;
            if paging.map(va, pa, flags, &mut m.frames).is_err() {
                // Roll back only the pages mapped by this call. Physical
                // frames are not ours, so they are never freed.
                for j in 0..i {
                    let _ = paging.unmap(MMIO_VIRT_BASE + j * FRAME_SIZE, &mut m.frames);
                }
                return Err(MmioError::MapFailed);
            }
        }
        Ok::<(), MmioError>(())
    })?;

    let virt = MMIO_VIRT_BASE + offset;
    let mut result = [0u8; 24];
    result[0..8].copy_from_slice(&virt.to_le_bytes());
    result[8..16].copy_from_slice(&paddr.to_le_bytes());
    result[16..24].copy_from_slice(&len.to_le_bytes());
    crate::syscall::copy_to_user(out_ptr, &result).map_err(|_| MmioError::BadOutPtr)?;

    serial_println!(
        "mmio: {:#x}..{:#x} -> {:#x} ({} page(s), uncached)",
        paddr,
        paddr + len,
        virt,
        pages
    );
    Ok(())
}
