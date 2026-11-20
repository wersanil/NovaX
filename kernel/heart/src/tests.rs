//! Kernel self-tests, compiled only with `--features test`.
//!
//! `run_all` runs synchronously on the BSP and exits QEMU on success; the
//! thread test is last because its workers never terminate.

use novax_memory::{MemoryError, NO_EXEC, PRESENT, WRITABLE};

use crate::{arch, memory_mut, sched, serial_println};
use core::sync::atomic::{AtomicU32, Ordering};

/// Run every kernel self-test in order. Panics on the first failure.
pub fn run_all() {
    serial_println!("=== NovaX kernel tests ===");
    test_frame_alloc_and_free();
    test_frame_alloc_1000();
    test_paging_map_unmap();
    test_heap_alloc();
    test_breakpoint();
    test_timer_ticks();
    test_threads(); // leaves workers running forever — must be last
    serial_println!("=== all tests passed ===");
}

static COUNTER_A: AtomicU32 = AtomicU32::new(0);
static COUNTER_B: AtomicU32 = AtomicU32::new(0);
static COUNTER_C: AtomicU32 = AtomicU32::new(0);

extern "C" fn worker_a() {
    loop {
        COUNTER_A.fetch_add(1, Ordering::Relaxed);
        sched::yield_now();
    }
}
extern "C" fn worker_b() {
    loop {
        COUNTER_B.fetch_add(1, Ordering::Relaxed);
        sched::yield_now();
    }
}
extern "C" fn worker_c() {
    loop {
        COUNTER_C.fetch_add(1, Ordering::Relaxed);
        sched::yield_now();
    }
}

fn test_frame_alloc_and_free() {
    serial_println!("[test] frame alloc/free basic");
    let before = memory_mut(|m| m.frames.free_count());
    let f = memory_mut(|m| m.frames.alloc()).expect("alloc one frame");
    let mid = memory_mut(|m| m.frames.free_count());
    assert_eq!(mid, before - 1, "free_count must decrease by 1");
    memory_mut(|m| m.frames.free(f));
    let after = memory_mut(|m| m.frames.free_count());
    assert_eq!(after, before, "free_count must be restored");
    serial_println!("    ok");
}

fn test_frame_alloc_1000() {
    serial_println!("[test] alloc 1000 frames, free, verify consistency");
    let before = memory_mut(|m| m.frames.free_count());

    let mut frames = alloc::vec::Vec::with_capacity(1000);
    for i in 0..1000 {
        let f = memory_mut(|m| m.frames.alloc())
            .unwrap_or_else(|| panic!("alloc failed at iteration {}", i));
        frames.push(f);
    }
    let mid = memory_mut(|m| m.frames.free_count());
    assert_eq!(mid, before - 1000, "free_count after 1000 allocs");

    // No duplicates: they are strictly increasing in alloc order.
    for w in frames.windows(2) {
        assert!(
            w[0].number < w[1].number,
            "allocator must return increasing frames"
        );
    }

    for f in frames.drain(..) {
        memory_mut(|m| m.frames.free(f));
    }
    let after = memory_mut(|m| m.frames.free_count());
    assert_eq!(after, before, "free_count restored after mass free");
    serial_println!("    ok");
}

fn test_paging_map_unmap() {
    serial_println!("[test] paging map/translate/unmap");
    // Use a virtual address in kernel space that nothing else uses.
    const TEST_VIRT: u64 = 0xffff_ffff_a000_0000;

    let (phys_before, present_before) = memory_mut(|m| {
        let f = m.frames.alloc().expect("temp frame");
        let p = f.start_address();
        let prev = m.paging.translate(TEST_VIRT);
        (p, prev)
    });
    assert!(
        present_before.is_none(),
        "test VA must be unmapped initially"
    );

    memory_mut(|m| {
        let frame = novax_memory::PhysFrame::containing(phys_before);
        m.paging
            .map(
                TEST_VIRT,
                frame.start_address(),
                PRESENT | WRITABLE | NO_EXEC,
                &mut m.frames,
            )
            .expect("map test page");
    });

    let translated = memory_mut(|m| m.paging.translate(TEST_VIRT)).expect("must translate");
    assert_eq!(
        translated & !0xFFF,
        phys_before & !0xFFF,
        "physical address must match"
    );

    let unmapped = memory_mut(|m| m.paging.unmap(TEST_VIRT, &mut m.frames)).expect("unmap");
    assert_eq!(
        unmapped & !0xFFF,
        phys_before & !0xFFF,
        "unmap returns the same physical base"
    );

    let after = memory_mut(|m| m.paging.translate(TEST_VIRT));
    assert!(after.is_none(), "must be gone after unmap");

    memory_mut(|m| {
        m.frames
            .free(novax_memory::PhysFrame::containing(phys_before))
    });

    // Double-unmap must error.
    let err = memory_mut(|m| m.paging.unmap(TEST_VIRT, &mut m.frames));
    assert!(
        matches!(err, Err(MemoryError::NotMapped)),
        "double unmap must be NotMapped"
    );

    serial_println!("    ok");
}

fn test_heap_alloc() {
    serial_println!("[test] heap alloc");
    use alloc::vec::Vec;

    let mut v: Vec<u64> = Vec::new();
    for i in 0..1024u64 {
        v.push(i * i);
    }
    for (i, x) in v.iter().enumerate() {
        assert_eq!(*x, (i as u64) * (i as u64));
    }
    drop(v);

    let s: alloc::string::String = (0..100).map(|_| 'x').collect();
    assert_eq!(s.len(), 100);
    serial_println!("    ok");
}

fn test_breakpoint() {
    serial_println!("[test] int3 -> breakpoint handler");
    // WHY unsafe: `int3` is a trap; the handler is installed and returns
    // normally, so control resumes right after this instruction.
    unsafe {
        core::arch::asm!("int3", options(nomem, nostack, preserves_flags));
    }
    serial_println!("    ok");
}

fn test_timer_ticks() {
    serial_println!("[test] lapic timer increments TICKS");
    use core::sync::atomic::Ordering;

    // Enable interrupts only for the duration of this test.
    // WHY unsafe: `sti`/`cli` are privileged; we're in ring 0.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
    }

    let start = arch::x86_64::TICKS.load(Ordering::Acquire);
    // Busy-wait until at least three ticks have elapsed or a bound is hit.
    let deadline = 50_000_000u64;
    let mut spins = 0u64;
    while arch::x86_64::TICKS.load(Ordering::Acquire) < start + 3 && spins < deadline {
        core::hint::spin_loop();
        spins += 1;
    }

    // SAFETY: `cli` is privileged; we are in ring 0 and the tick check
    // that follows must observe a stable counter.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
    }

    let end = arch::x86_64::TICKS.load(Ordering::Acquire);
    assert!(end >= start + 3, "expected >=3 ticks, got {}", end - start);
    serial_println!("    ok ({} ticks)", end - start);
}

fn test_threads() {
    serial_println!("[test] spawn 3 workers, run 5 ticks, verify all ran");

    // Reset, in case something ran before.
    COUNTER_A.store(0, Ordering::Relaxed);
    COUNTER_B.store(0, Ordering::Relaxed);
    COUNTER_C.store(0, Ordering::Relaxed);

    sched::spawn("a", worker_a);
    sched::spawn("b", worker_b);
    sched::spawn("c", worker_c);

    let start = arch::x86_64::TICKS.load(Ordering::Acquire);
    while arch::x86_64::TICKS
        .load(Ordering::Acquire)
        .wrapping_sub(start)
        < 5
    {
        sched::yield_now();
    }

    let a = COUNTER_A.load(Ordering::Relaxed);
    let b = COUNTER_B.load(Ordering::Relaxed);
    let c = COUNTER_C.load(Ordering::Relaxed);
    assert!(
        a > 0 && b > 0 && c > 0,
        "all workers must run at least once: a={a} b={b} c={c}"
    );
    serial_println!("    ok (a={}, b={}, c={})", a, b, c);
}
