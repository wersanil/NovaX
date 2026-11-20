//! Kernel heap: 1 MiB of mapped frames managed by `linked_list_allocator`.
//!
//! The heap is mapped at a fixed kernel virtual address and backed by
//! frames from the global allocator; it is the only dynamic allocator in
//! the kernel (`alloc::Vec`, `Box`, ...).

use core::sync::atomic::{AtomicBool, Ordering};

use linked_list_allocator::LockedHeap;
use novax_memory::{MemoryError, MemoryResult, NO_EXEC, PRESENT, WRITABLE};

use crate::{serial_println, MemoryManager, MEMORY};

/// First kernel virtual address of the heap window.
pub const HEAP_VIRT_BASE: u64 = 0xffff_ffff_9000_0000;
/// Total heap size in bytes.
pub const HEAP_SIZE: u64 = 1024 * 1024; // 1 MiB
/// Number of 4 KiB frames backing the heap.
pub const HEAP_FRAMES: u64 = HEAP_SIZE / novax_memory::FRAME_SIZE;

/// Global allocator used by `alloc` collections.
#[global_allocator]
pub static HEAP: LockedHeap = LockedHeap::empty();

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Map the heap frames and initialise the global allocator. Idempotent.
pub fn init() -> MemoryResult<()> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }

    let mut guard = MEMORY.get().expect("MEMORY unset before heap::init").lock();

    // Split the manager into two disjoint &mut borrows so we can pass the
    // frame allocator into paging.map without aliasing the whole struct.
    let MemoryManager { frames, paging } = &mut *guard;

    for i in 0..HEAP_FRAMES {
        let frame = frames.alloc().ok_or(MemoryError::OutOfFrames)?;
        let virt = HEAP_VIRT_BASE + i * novax_memory::FRAME_SIZE;
        let flags = PRESENT | WRITABLE | NO_EXEC;
        paging.map(virt, frame.start_address(), flags, frames)?;
    }

    drop(guard);

    // WHY unsafe: LockedHeap::init requires a valid, page-aligned,
    // non-aliased region. We just mapped one and own it exclusively.
    unsafe {
        HEAP.lock()
            .init(HEAP_VIRT_BASE as *mut u8, HEAP_SIZE as usize);
    }

    serial_println!("heap:  {} KiB @ {:#x}", HEAP_SIZE / 1024, HEAP_VIRT_BASE);
    Ok(())
}
