//! Kernel self-tests, compiled only with `--features test`.
//!
//! `run_all` runs synchronously on the BSP and exits QEMU on success; the
//! thread test is last because its workers never terminate.

use novax_memory::{MemoryError, NO_EXEC, PRESENT, USER, WRITABLE};

use crate::cdt;
use crate::ipc::{self, Message, RecvOutcome};
use crate::user::{self, AddressSpace};
use crate::{arch, memory_mut, sched, serial_println};
use core::sync::atomic::{AtomicU32, Ordering};

/// Run every kernel self-test in order. Panics on the first failure.
pub fn run_all() {
    serial_println!("=== NovaX kernel tests ===");
    test_frame_alloc_and_free();
    test_frame_alloc_1000();
    test_paging_map_unmap();
    test_user_range_check();
    test_heap_alloc();
    test_ipc_queue_bound();
    test_cdt_mint_revoke();
    test_notification_bits();
    test_elf_loader();
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

fn test_user_range_check() {
    serial_println!("[test] user range check honours page flags");
    const VA: u64 = 0x0000_5000_0000;

    let (user_frame, super_frame) = memory_mut(|m| {
        let user = m.frames.alloc().expect("user frame");
        let supervisor = m.frames.alloc().expect("supervisor frame");
        m.paging
            .map(
                VA,
                user.start_address(),
                PRESENT | USER | WRITABLE | NO_EXEC,
                &mut m.frames,
            )
            .expect("map user page");
        m.paging
            .map(
                VA + 0x1000,
                supervisor.start_address(),
                PRESENT | WRITABLE | NO_EXEC,
                &mut m.frames,
            )
            .expect("map supervisor page");
        (user, supervisor)
    });

    memory_mut(|m| {
        assert!(
            m.paging.check_user_range(VA, 16, false),
            "user page must be readable"
        );
        assert!(
            m.paging.check_user_range(VA, 16, true),
            "user page must be writable"
        );
        assert!(
            m.paging.check_user_range(VA, 0x1000, true),
            "exactly one page is fine"
        );
        assert!(
            !m.paging.check_user_range(VA, 0x1001, true),
            "a range crossing into a supervisor page must fail"
        );
        assert!(
            !m.paging.check_user_range(VA + 0x1000, 16, false),
            "supervisor page is not user-accessible"
        );
        assert!(
            !m.paging.check_user_range(VA + 0x2000, 16, false),
            "unmapped page must fail"
        );
        let _ = m.paging.unmap(VA, &mut m.frames);
        let _ = m.paging.unmap(VA + 0x1000, &mut m.frames);
        m.frames.free(user_frame);
        m.frames.free(super_frame);
    });

    serial_println!("    ok");
}

fn test_ipc_queue_bound() {
    serial_println!("[test] ipc queue is bounded and recoverable");
    let ep = ipc::create_endpoint();
    let msg = Message::plain([0u8; ipc::MAX_MSG_LEN], 0, 0);

    for i in 0..ipc::MAX_QUEUE {
        assert!(ipc::send(ep, msg), "queue must accept message {i}");
    }
    assert!(
        !ipc::send(ep, msg),
        "queue must reject the message after MAX_QUEUE"
    );

    for _ in 0..ipc::MAX_QUEUE {
        assert!(
            matches!(ipc::recv(ep), RecvOutcome::Message(_)),
            "queued message must be returned"
        );
    }
    assert!(ipc::send(ep, msg), "queue must accept after draining");
    assert!(matches!(ipc::recv(ep), RecvOutcome::Message(_)));

    serial_println!("    ok");
}

fn test_cdt_mint_revoke() {
    serial_println!("[test] CDT mint/revoke tree");
    let root = cdt::alloc_root(999, 0, ipc::CapKind::Endpoint(0), ipc::CAP_READ).expect("root");
    let child =
        cdt::alloc_child(root, 999, 1, ipc::CapKind::Endpoint(0), ipc::CAP_READ).expect("child");
    let grandchild = cdt::alloc_child(child, 999, 2, ipc::CapKind::Endpoint(0), ipc::CAP_READ)
        .expect("grandchild");
    assert!(cdt::exists(root) && cdt::exists(child) && cdt::exists(grandchild));
    assert_eq!(cdt::revoke_children(root), 2, "two descendants");
    assert!(cdt::exists(root), "root must survive revoke");
    assert!(!cdt::exists(child) && !cdt::exists(grandchild));
    assert_eq!(cdt::revoke_children(root), 0, "revoke is idempotent");
    cdt::free_node(root);
    assert!(!cdt::exists(root));

    // `revoke_owned` removes roots and descendants owned by the thread.
    let root = cdt::alloc_root(1000, 0, ipc::CapKind::Endpoint(0), ipc::CAP_READ).unwrap();
    let _ = cdt::alloc_child(root, 1000, 1, ipc::CapKind::Endpoint(0), ipc::CAP_READ);
    assert_eq!(cdt::revoke_owned(1000), 2);
    assert!(!cdt::exists(root));
    serial_println!("    ok");
}

fn test_notification_bits() {
    serial_println!("[test] notification fetch_or/swap");
    let nid = ipc::create_notification().expect("notification");
    assert_eq!(ipc::notify_poll(nid), Some(0));
    assert!(ipc::notify_signal(nid, 0x2));
    assert!(ipc::notify_signal(nid, 0x5));
    assert_eq!(ipc::notify_poll(nid), Some(0x7));
    assert_eq!(ipc::notify_poll(nid), Some(0));
    serial_println!("    ok");
}

fn test_elf_loader() {
    serial_println!("[test] ELF loader rejects malformed input without leaks");
    let before = memory_mut(|m| m.frames.free_count());
    let space = AddressSpace::new().expect("address space");

    let valid = valid_test_elf();
    let ph = 0x40usize;

    let mut bad = valid.clone();
    bad[54..56].copy_from_slice(&0u16.to_le_bytes()); // e_phentsize = 0
    assert!(matches!(
        user::load_elf(&space, &bad),
        Err(user::UserError::Malformed)
    ));

    let mut bad = valid.clone();
    bad[32..40].copy_from_slice(&0x1000u64.to_le_bytes()); // phoff past EOF
    assert!(matches!(
        user::load_elf(&space, &bad),
        Err(user::UserError::Malformed)
    ));

    let mut bad = valid.clone();
    bad[ph + 32..ph + 40].copy_from_slice(&2u64.to_le_bytes()); // filesz > memsz
    assert!(matches!(
        user::load_elf(&space, &bad),
        Err(user::UserError::Malformed)
    ));

    let mut bad = valid.clone();
    bad[ph + 8..ph + 16].copy_from_slice(&0x100u64.to_le_bytes()); // p_offset past EOF
    assert!(matches!(
        user::load_elf(&space, &bad),
        Err(user::UserError::Malformed)
    ));

    let mut bad = valid.clone();
    bad[ph + 16..ph + 24].copy_from_slice(&0xffff_ffff_8000_0000u64.to_le_bytes()); // kernel VA
    assert!(matches!(
        user::load_elf(&space, &bad),
        Err(user::UserError::Malformed)
    ));

    let mut bad = valid.clone();
    bad[24..32].copy_from_slice(&0u64.to_le_bytes()); // e_entry = 0
    assert!(matches!(
        user::load_elf(&space, &bad),
        Err(user::UserError::NoEntry)
    ));

    let entry = user::load_elf(&space, &valid).expect("valid ELF must load");
    assert_eq!(entry, 0x40_0000);

    space.destroy();
    let after = memory_mut(|m| m.frames.free_count());
    assert_eq!(after, before, "ELF load + destroy must not leak frames");

    serial_println!("    ok");
}

/// Minimal valid ELF64: one R+X PT_LOAD at 0x40_0000 with one file byte.
fn valid_test_elf() -> alloc::vec::Vec<u8> {
    const PHOFF: usize = 0x40;
    const DATA: usize = 0x78;

    let mut elf = alloc::vec![0u8; 0x100];
    elf[0..4].copy_from_slice(b"\x7FELF");
    elf[4] = 2; // ELFCLASS64
    elf[5] = 1; // ELFDATA2LSB
    elf[6] = 1; // EV_CURRENT
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    elf[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
    elf[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    elf[24..32].copy_from_slice(&0x40_0000u64.to_le_bytes()); // e_entry
    elf[32..40].copy_from_slice(&(PHOFF as u64).to_le_bytes()); // e_phoff
    elf[52..54].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
    elf[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
    elf[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum

    elf[PHOFF..PHOFF + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    elf[PHOFF + 4..PHOFF + 8].copy_from_slice(&5u32.to_le_bytes()); // PF_R | PF_X
    elf[PHOFF + 8..PHOFF + 16].copy_from_slice(&(DATA as u64).to_le_bytes());
    elf[PHOFF + 16..PHOFF + 24].copy_from_slice(&0x40_0000u64.to_le_bytes()); // p_vaddr
    elf[PHOFF + 24..PHOFF + 32].copy_from_slice(&0x40_0000u64.to_le_bytes()); // p_paddr
    elf[PHOFF + 32..PHOFF + 40].copy_from_slice(&1u64.to_le_bytes()); // p_filesz
    elf[PHOFF + 40..PHOFF + 48].copy_from_slice(&1u64.to_le_bytes()); // p_memsz
    elf[PHOFF + 48..PHOFF + 56].copy_from_slice(&0x1000u64.to_le_bytes()); // p_align
    elf[DATA] = 0xC3; // ret
    elf
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
