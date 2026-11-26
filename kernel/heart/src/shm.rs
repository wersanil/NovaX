//! Read-only shared buffers.
//!
//! A [`crate::ipc::CapKind::Shm`] capability is derived from an allocated
//! DMA buffer and maps the same physical frames into another process at a
//! fixed user virtual address. Revoking the capability unmaps the window
//! from every holder.

use novax_memory::{PageTableRef, FRAME_SIZE, NO_EXEC, PRESENT, USER};

use crate::ipc::Capability;
use crate::sched::ThreadId;
use crate::{memory_mut, sched, serial_println};
use spin::Mutex;

/// Fixed user virtual address of the shared-buffer window.
pub const SHM_VIRT_BASE: u64 = 0x0000_5000_0000;
/// Upper bound on one shared window.
pub const MAX_SHM_LEN: u64 = 1024 * FRAME_SIZE;

/// Why a shared mapping failed.
#[derive(Debug)]
pub enum ShmError {
    /// Kernel threads have no address space.
    NotUserThread,
    /// Zero-length, too large or unaligned-in-a-bad-way request.
    BadLength,
    /// The caller already has a shared window mapped.
    AlreadyMapped,
    /// A page in the window was already mapped.
    MapFailed,
    /// The result structure could not be written.
    BadOutPtr,
}

#[derive(Clone, Copy)]
struct ShmMap {
    owner: ThreadId,
    pml4: u64,
    len: u64,
}

static MAPS: Mutex<[Option<ShmMap>; 16]> = Mutex::new([None; 16]);

/// Map the shared buffer into the calling process and write
/// `{virt, phys, len}` (three little-endian `u64`s) to `out_ptr`.
pub fn map_for_current(phys: u64, len: u64, out_ptr: u64) -> Result<(), ShmError> {
    if len == 0 || len > MAX_SHM_LEN || phys == 0 {
        return Err(ShmError::BadLength);
    }
    let pml4 = sched::current_pml4_phys();
    if pml4 == 0 {
        return Err(ShmError::NotUserThread);
    }
    if !crate::syscall::user_range_ok(out_ptr, 24, true) {
        return Err(ShmError::BadOutPtr);
    }
    let owner = sched::current_id();
    {
        let maps = MAPS.lock();
        if maps
            .iter()
            .flatten()
            .any(|m| m.owner == owner || m.pml4 == pml4)
        {
            return Err(ShmError::AlreadyMapped);
        }
    }

    let pages = len.div_ceil(FRAME_SIZE);
    memory_mut(|m| {
        let hhdm = m.frames.hhdm();
        let mut paging = PageTableRef::from_phys(pml4, hhdm);
        for i in 0..pages {
            let va = SHM_VIRT_BASE + i * FRAME_SIZE;
            let pa = (phys & !(FRAME_SIZE - 1)) + i * FRAME_SIZE;
            if paging
                .map(va, pa, PRESENT | USER | NO_EXEC, &mut m.frames)
                .is_err()
            {
                for j in 0..i {
                    let _ = paging.unmap(SHM_VIRT_BASE + j * FRAME_SIZE, &mut m.frames);
                }
                return Err(ShmError::MapFailed);
            }
        }
        Ok::<(), ShmError>(())
    })?;

    let mut slot = MAPS.lock();
    let Some(entry) = slot.iter_mut().find(|m| m.is_none()) else {
        return Err(ShmError::MapFailed);
    };
    *entry = Some(ShmMap { owner, pml4, len });
    drop(slot);

    let mut result = [0u8; 24];
    result[0..8].copy_from_slice(&SHM_VIRT_BASE.to_le_bytes());
    result[8..16].copy_from_slice(&phys.to_le_bytes());
    result[16..24].copy_from_slice(&len.to_le_bytes());
    crate::syscall::copy_to_user(out_ptr, &result).map_err(|_| ShmError::BadOutPtr)?;

    serial_println!(
        "shm: {:#x}..{:#x} -> {:#x} ({} page(s), read-only)",
        phys,
        phys + len,
        SHM_VIRT_BASE,
        pages
    );
    Ok(())
}

/// Unmap every shared window owned by `owner`.
pub fn unmap_for(owner: ThreadId, pml4: u64) {
    let mut removed = [None; 16];
    {
        let mut maps = MAPS.lock();
        for slot in maps.iter_mut() {
            if let Some(m) = *slot {
                if m.owner == owner {
                    *slot = None;
                    if let Some(free) = removed.iter_mut().find(|r| r.is_none()) {
                        *free = Some(m);
                    }
                }
            }
        }
    }
    for m in removed.into_iter().flatten() {
        let pages = m.len.div_ceil(FRAME_SIZE);
        memory_mut(|mem| {
            let hhdm = mem.frames.hhdm();
            let mut paging = PageTableRef::from_phys(m.pml4, hhdm);
            for i in 0..pages {
                let _ = paging.unmap(SHM_VIRT_BASE + i * FRAME_SIZE, &mut mem.frames);
            }
        });
        crate::arch::x86_64::flush_tlb_remote(owner, m.pml4);
    }
    serial_println!("shm: revoked window of thread {} (pml4={:#x})", owner, pml4);
}

/// Map a capability's shared buffer. Rejects non-`Shm` kinds.
pub fn map_cap(cap: Capability, out_ptr: u64) -> Result<(), ShmError> {
    match cap.kind {
        crate::ipc::CapKind::Shm { phys, len } => map_for_current(phys, len as u64, out_ptr),
        _ => Err(ShmError::BadLength),
    }
}
