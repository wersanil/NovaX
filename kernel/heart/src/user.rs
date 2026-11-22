//! User-mode support: ELF loader, address spaces, and the trampoline
//! that transitions from kernel to ring 3.
#![allow(dead_code)] // модуль живёт только в non-test сборке

use alloc::vec::Vec;

use novax_memory::{
    paging::HUGE, FrameAllocator, HhdmOffset, MemoryError, PageTableRef, PhysFrame, ADDR_MASK,
    FRAME_SIZE, NO_EXEC, PRESENT, USER, WRITABLE,
};

use crate::{dma::DMA_VIRT_BASE, memory_mut, mmio::MMIO_VIRT_BASE, sched, serial_println};

/// Top of the user stack. Must be in the lower half and page-aligned
/// minus 16, so that a `call` from user code has RSP % 16 == 0.
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_0000;
const USER_STACK_PAGES: u64 = 2;
/// First user virtual address a PT_LOAD segment may use. Page 0 stays
/// unmapped so a NULL dereference is a user fault, not a valid access.
const USER_MIN: u64 = FRAME_SIZE;
/// Exclusive upper bound of the user half.
const USER_MAX: u64 = 0x0000_8000_0000_0000;
/// First page of the user stack window.
const USER_STACK_START: u64 = USER_STACK_TOP - USER_STACK_PAGES * FRAME_SIZE;
/// Maximum number of PT_LOAD segments accepted from one ELF.
const MAX_SEGMENTS: usize = 64;
/// Size of an ELF64 program header.
const PHDR_SIZE: usize = 56;
/// Program header type: loadable segment.
const PT_LOAD: u32 = 1;
/// Program header flags.
const PF_X: u64 = 1;
const PF_W: u64 = 2;
/// Fixed windows reserved for runtime mappings; an ELF image must not
/// overlap them or DMA/MMIO would fail after the process starts.
const DMA_MAX_LEN: u64 = 1024 * FRAME_SIZE;
const MMIO_MAX_LEN: u64 = 256 * 1024 * 1024 + FRAME_SIZE;

/// Errors from loading or launching a user program.
#[derive(Debug)]
pub enum UserError {
    /// Missing or wrong ELF magic.
    NotElf,
    /// Not a 64-bit ELF.
    NotElf64,
    /// Big-endian ELF (unsupported).
    NotLittleEndian,
    /// Structurally invalid ELF: truncated headers, out-of-range fields,
    /// or segments outside the user address space.
    Malformed,
    /// Wraps a lower-level error from `novax_memory`. The field is used
    /// for its `Debug` output only.
    Memory(#[allow(dead_code)] MemoryError),
    /// ELF has no entry point inside a mapped segment.
    NoEntry,
}

impl From<MemoryError> for UserError {
    fn from(e: MemoryError) -> Self {
        UserError::Memory(e)
    }
}

/// One validated PT_LOAD segment.
struct Segment {
    offset: u64,
    vaddr: u64,
    filesz: u64,
    start: u64,
    end: u64,
    flags: u64,
}

/// A private user address space: PML4 with a cloned kernel half.
pub struct AddressSpace {
    /// Physical frame holding the PML4.
    pub pml4: PhysFrame,
}

impl AddressSpace {
    /// Allocate a fresh address space: private PML4 with the kernel
    /// half cloned from the boot PML4 and the user half cleared.
    pub fn new() -> Result<Self, UserError> {
        memory_mut(|m| {
            let pml4 = m.frames.alloc().ok_or(MemoryError::OutOfFrames)?;
            let hhdm = m.frames.hhdm();

            let dst = (hhdm.as_u64() + pml4.start_address()) as *mut u64;
            let src = (hhdm.as_u64() + crate::arch::x86_64::kernel_pml4_phys()) as *const u64;

            // WHY unsafe: both pointers are valid HHDM addresses within
            // freshly-allocated or already-owned pages; we copy a 4 KiB
            // page's worth of u64s.
            unsafe {
                for i in 0..256u64 {
                    core::ptr::write(dst.add(i as usize), 0);
                }
                for i in 256..512u64 {
                    core::ptr::write(dst.add(i as usize), core::ptr::read(src.add(i as usize)));
                }
            }

            Ok(AddressSpace { pml4 })
        })
    }

    /// Physical address of this address space's PML4.
    pub fn pml4_phys(&self) -> u64 {
        self.pml4.start_address()
    }

    /// Free every user-half frame and page table owned by this address
    /// space, including the PML4 itself.
    ///
    /// Must only be called for a space that never became the address space
    /// of a running thread (for example after a failed `load_elf`).
    pub fn destroy(&self) {
        memory_mut(|m| {
            let hhdm = m.frames.hhdm();
            free_user_half(hhdm, &mut m.frames, self.pml4.start_address());
        });
    }
}

/// Load a static ELF64 with only PT_LOAD segments. Returns the entry RIP.
///
/// Every header is validated before the frame allocator is touched, so a
/// malformed image is rejected without leaving partial mappings behind.
pub fn load_elf(space: &AddressSpace, elf: &[u8]) -> Result<u64, UserError> {
    if elf.len() < 64 || &elf[0..4] != b"\x7FELF" {
        return Err(UserError::NotElf);
    }
    if elf[4] != 2 {
        return Err(UserError::NotElf64);
    }
    if elf[5] != 1 {
        return Err(UserError::NotLittleEndian);
    }

    let entry = rd_u64(elf, 24);
    if !(USER_MIN..USER_MAX).contains(&entry) {
        return Err(UserError::NoEntry);
    }

    let phoff = rd_u64(elf, 32);
    let phentsize = rd_u16(elf, 54) as u64;
    let phnum = rd_u16(elf, 56) as u64;

    if phentsize < PHDR_SIZE as u64 || phnum == 0 || phnum > MAX_SEGMENTS as u64 {
        return Err(UserError::Malformed);
    }
    let ph_end = phoff
        .checked_add(phnum.checked_mul(phentsize).ok_or(UserError::Malformed)?)
        .ok_or(UserError::Malformed)?;
    if ph_end > elf.len() as u64 {
        return Err(UserError::Malformed);
    }

    let mut segments: Vec<Segment> = Vec::new();
    for i in 0..phnum {
        let off = (phoff + i * phentsize) as usize;
        let ph = &elf[off..off + phentsize as usize];
        if rd_u32(ph, 0) != PT_LOAD {
            continue;
        }
        let flags = rd_u32(ph, 4) as u64;
        let offset = rd_u64(ph, 8);
        let vaddr = rd_u64(ph, 16);
        let filesz = rd_u64(ph, 32);
        let memsz = rd_u64(ph, 40);

        if filesz > memsz {
            return Err(UserError::Malformed);
        }
        let file_end = offset.checked_add(filesz).ok_or(UserError::Malformed)?;
        if file_end > elf.len() as u64 {
            return Err(UserError::Malformed);
        }
        let vaddr_end = vaddr.checked_add(memsz).ok_or(UserError::Malformed)?;

        let (start, end) = if memsz == 0 {
            (0u64, 0u64)
        } else {
            let start = vaddr & !(FRAME_SIZE - 1);
            let end = vaddr_end
                .checked_add(FRAME_SIZE - 1)
                .ok_or(UserError::Malformed)?
                & !(FRAME_SIZE - 1);
            if start < USER_MIN || end > USER_STACK_START {
                return Err(UserError::Malformed);
            }
            if overlaps(start, end, DMA_VIRT_BASE, DMA_VIRT_BASE + DMA_MAX_LEN)
                || overlaps(start, end, MMIO_VIRT_BASE, MMIO_VIRT_BASE + MMIO_MAX_LEN)
            {
                return Err(UserError::Malformed);
            }
            (start, end)
        };

        segments.push(Segment {
            offset,
            vaddr,
            filesz,
            start,
            end,
            flags,
        });
    }

    memory_mut(|m| {
        let hhdm = m.frames.hhdm();
        let mut paging = PageTableRef::from_phys(space.pml4_phys(), hhdm);

        for seg in &segments {
            if seg.start == seg.end {
                continue;
            }

            let mut flags = PRESENT | USER;
            if seg.flags & PF_W != 0 {
                flags |= WRITABLE;
            }
            if seg.flags & PF_X == 0 {
                flags |= NO_EXEC;
            }

            for page in (seg.start..seg.end).step_by(FRAME_SIZE as usize) {
                let frame = m.frames.alloc().ok_or(MemoryError::OutOfFrames)?;
                let dst = (hhdm.as_u64() + frame.start_address()) as *mut u8;
                // WHY unsafe: freshly-allocated frame, full-page zeroing.
                unsafe {
                    core::ptr::write_bytes(dst, 0, FRAME_SIZE as usize);
                }

                match paging.map(page, frame.start_address(), flags, &mut m.frames) {
                    Ok(()) => {}
                    Err(MemoryError::AlreadyMapped) => {
                        // ELF segments can share a page boundary (e.g. .text
                        // and .rodata in the same 4 KiB page). The frame is
                        // already mapped — free the one we just allocated.
                        m.frames.free(frame);
                    }
                    Err(e) => {
                        m.frames.free(frame);
                        return Err(e.into());
                    }
                }
            }

            // Copy file bytes page by page: one page-table walk per page
            // instead of one per byte.
            let mut copied = 0u64;
            while copied < seg.filesz {
                let vaddr = seg.vaddr + copied;
                let phys = paging.translate(vaddr).ok_or(MemoryError::NotMapped)?;
                let page_off = (vaddr & (FRAME_SIZE - 1)) as usize;
                let n = core::cmp::min(
                    FRAME_SIZE as usize - page_off,
                    (seg.filesz - copied) as usize,
                );
                let src_off = (seg.offset + copied) as usize;
                let dst = (hhdm.as_u64() + phys) as *mut u8;
                // WHY unsafe: `src_off..src_off + n` lies inside the
                // validated ELF buffer and `dst..dst + n` lies inside one
                // mapped user page (the length is clamped to the page).
                unsafe {
                    core::ptr::copy_nonoverlapping(elf[src_off..src_off + n].as_ptr(), dst, n);
                }
                copied += n as u64;
            }
        }

        // The entry point must resolve in the loaded image; otherwise the
        // first ring-3 instruction would fault.
        if paging.translate(entry).is_none() {
            return Err(UserError::NoEntry);
        }

        // User stack.
        let flags = PRESENT | USER | WRITABLE | NO_EXEC;
        for page in (USER_STACK_START..USER_STACK_TOP).step_by(FRAME_SIZE as usize) {
            let frame = m.frames.alloc().ok_or(MemoryError::OutOfFrames)?;
            let dst = (hhdm.as_u64() + frame.start_address()) as *mut u8;
            // SAFETY: freshly allocated frame, full-page zeroing.
            unsafe {
                core::ptr::write_bytes(dst, 0, FRAME_SIZE as usize);
            }
            if let Err(e) = paging.map(page, frame.start_address(), flags, &mut m.frames) {
                m.frames.free(frame);
                return Err(e.into());
            }
        }

        Ok(entry)
    })
}

/// Load the embedded user program into a fresh address space and spawn
/// a user thread to run it.
pub fn launch(
    elf: &[u8],
    cspace: [Option<crate::ipc::Capability>; 16],
) -> Result<sched::ThreadId, UserError> {
    let space = AddressSpace::new()?;
    let entry = match load_elf(&space, elf) {
        Ok(entry) => entry,
        Err(e) => {
            space.destroy();
            return Err(e);
        }
    };

    let tid = sched::spawn_user(
        "hello",
        space.pml4_phys(),
        entry,
        USER_STACK_TOP - 16,
        cspace,
    );

    serial_println!(
        "user: elf loaded, entry={:#x}, pml4={:#x}",
        entry,
        space.pml4_phys()
    );
    Ok(tid)
}

/// Trampoline: entered via `switch_context` on the first run of a
/// user thread. Reads `entry`/`stack` from the current TCB and hands
/// control to `enter_userspace`.
#[no_mangle]
pub extern "C" fn user_trampoline() -> ! {
    let (entry, stack) = {
        let tcb = sched::current_tcb_snapshot();
        (tcb.user_entry, tcb.user_stack)
    };
    serial_println!("user: entering ring 3 at rip={:#x} rsp={:#x}", entry, stack);
    // WHY unsafe: enter_userspace's safety contract is guaranteed by the
    // caller of spawn_user: PML4 loaded, entry/stack mapped.
    unsafe { crate::arch::x86_64::enter_userspace(entry, stack) }
}

// --- Address-space teardown -------------------------------------------------

/// Free the user half of `pml4_phys` and the PML4 frame itself. Kernel
/// half entries are shared with every address space and are left alone.
fn free_user_half(hhdm: HhdmOffset, frames: &mut FrameAllocator, pml4_phys: u64) {
    if pml4_phys == 0 {
        return;
    }
    for idx in 0..256u64 {
        let entry = read_entry(hhdm, pml4_phys, idx);
        if entry & PRESENT == 0 {
            continue;
        }
        // PML4 entries are never huge pages.
        free_table(hhdm, frames, entry & ADDR_MASK, 3);
    }
    frames.free(PhysFrame::containing(pml4_phys));
}

/// Free a page-table tree. `level` is 3 for a PDPT, 2 for a PD and 1 for
/// a PT; at level 1 the entries point at data frames.
fn free_table(hhdm: HhdmOffset, frames: &mut FrameAllocator, table_phys: u64, level: u8) {
    for idx in 0..512u64 {
        let entry = read_entry(hhdm, table_phys, idx);
        if entry & PRESENT == 0 {
            continue;
        }
        if level >= 2 && entry & HUGE != 0 {
            // Not ours to free: a huge mapping is never created for user
            // memory, and its "frame" is the whole huge page.
            continue;
        }
        let child = entry & ADDR_MASK;
        if level == 1 {
            frames.free(PhysFrame::containing(child));
        } else {
            free_table(hhdm, frames, child, level - 1);
        }
    }
    frames.free(PhysFrame::containing(table_phys));
}

fn read_entry(hhdm: HhdmOffset, table_phys: u64, idx: u64) -> u64 {
    // WHY unsafe: `table_phys` came from a present entry of a PML4 we own;
    // the HHDM maps it for as long as the kernel runs.
    unsafe { core::ptr::read_volatile((hhdm.as_u64() + table_phys + idx * 8) as *const u64) }
}

fn overlaps(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
    a0 < b1 && b0 < a1
}

// --- LE readers -------------------------------------------------------------

fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}
