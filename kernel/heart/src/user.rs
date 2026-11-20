//! User-mode support: ELF loader, address spaces, and the trampoline
//! that transitions from kernel to ring 3.
#![allow(dead_code)] // модуль живёт только в non-test сборке

use novax_memory::{
    MemoryError, PageTableRef, PhysFrame, FRAME_SIZE, NO_EXEC, PRESENT, USER, WRITABLE,
};

use crate::{memory_mut, sched, serial_println};

/// Top of the user stack. Must be in the lower half and page-aligned
/// minus 16, so that a `call` from user code has RSP % 16 == 0.
const USER_STACK_TOP: u64 = 0x0000_7fff_ffff_0000;
const USER_STACK_PAGES: u64 = 2;

/// Errors from loading or launching a user program.
#[derive(Debug)]
pub enum UserError {
    /// Missing or wrong ELF magic.
    NotElf,
    /// Not a 64-bit ELF.
    NotElf64,
    /// Big-endian ELF (unsupported).
    NotLittleEndian,
    /// Wraps a lower-level error from `novax_memory`. The field is used
    /// for its `Debug` output only.
    Memory(#[allow(dead_code)] MemoryError),
    /// ELF has no entry point.
    NoEntry,
}

impl From<MemoryError> for UserError {
    fn from(e: MemoryError) -> Self {
        UserError::Memory(e)
    }
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
}

/// Load a static ELF64 with only PT_LOAD segments. Returns the entry RIP.
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
    let phoff = rd_u64(elf, 32) as usize;
    let phentsize = rd_u16(elf, 54) as usize;
    let phnum = rd_u16(elf, 56) as usize;

    if entry == 0 {
        return Err(UserError::NoEntry);
    }

    memory_mut(|m| {
        let hhdm = m.frames.hhdm();
        let mut paging = PageTableRef::from_phys(space.pml4_phys(), hhdm);

        for i in 0..phnum {
            let ph = &elf[phoff + i * phentsize..];
            let p_type = rd_u32(ph, 0);
            if p_type != 1 {
                continue;
            } // PT_LOAD
            let p_flags = rd_u32(ph, 4);
            let p_offset = rd_u64(ph, 8);
            let p_vaddr = rd_u64(ph, 16);
            let p_filesz = rd_u64(ph, 32);
            let p_memsz = rd_u64(ph, 40);

            let start = p_vaddr & !(FRAME_SIZE - 1);
            let end = (p_vaddr + p_memsz + FRAME_SIZE - 1) & !(FRAME_SIZE - 1);

            let mut flags = PRESENT | USER;
            if p_flags & 2 != 0 {
                flags |= WRITABLE;
            } // PF_W
            if p_flags & 1 == 0 {
                flags |= NO_EXEC;
            } // !PF_X

            for page in (start..end).step_by(FRAME_SIZE as usize) {
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
                    Err(e) => return Err(e.into()),
                }
            }

            // Copy file bytes into mapped pages.
            for i in 0..p_filesz as usize {
                let vaddr = p_vaddr + i as u64;
                let phys = paging.translate(vaddr).ok_or(MemoryError::NotMapped)?;
                let dst = (hhdm.as_u64() + phys) as *mut u8;
                // WHY unsafe: address derived from our own PML4 walk.
                unsafe {
                    core::ptr::write_volatile(dst, elf[p_offset as usize + i]);
                }
            }
        }

        // User stack.
        let stack_start = USER_STACK_TOP - USER_STACK_PAGES * FRAME_SIZE;
        let flags = PRESENT | USER | WRITABLE | NO_EXEC;
        for page in (stack_start..USER_STACK_TOP).step_by(FRAME_SIZE as usize) {
            let frame = m.frames.alloc().ok_or(MemoryError::OutOfFrames)?;
            let dst = (hhdm.as_u64() + frame.start_address()) as *mut u8;
            // SAFETY: freshly allocated frame, full-page zeroing.
            unsafe {
                core::ptr::write_bytes(dst, 0, FRAME_SIZE as usize);
            }
            paging.map(page, frame.start_address(), flags, &mut m.frames)?;
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
    let entry = load_elf(&space, elf)?;

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
