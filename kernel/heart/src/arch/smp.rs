//! SMP support: per-CPU state, logical CPU discovery and AP bootstrap.
//!
//! Each logical CPU owns one `PerCpu` slot. The slot address is installed
//! in `IA32_GS_BASE`, so hot paths (syscall entry, scheduler) can reach
//! per-CPU data without locks. User mode never touches GS; `CR4.FSGSBASE`
//! stays disabled, so the kernel pointer is not readable from ring 3.

use core::arch::asm;
#[cfg(not(feature = "test"))]
use core::arch::naked_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(not(feature = "test"))]
use limine::mp::MpInfo;
use x86_64::structures::gdt::GlobalDescriptorTable;
use x86_64::structures::tss::TaskStateSegment;

/// Maximum number of logical CPUs supported by the per-CPU table.
pub const MAX_CPUS: usize = 8;

const BOOT_STACK_SIZE: usize = 32 * 1024;
const DF_STACK_SIZE: usize = 8 * 1024;

/// A stack with a guaranteed 16-byte alignment.
#[repr(align(16))]
pub struct AlignedStack<const N: usize>(pub [u8; N]);

/// One CPU's private state.
///
/// The first four fields are addressed directly from `syscall_entry`
/// through `gs:` and must keep their offsets (checked below).
#[repr(C, align(64))]
pub struct PerCpu {
    /// Pointer to this structure (used by AP bring-up).
    pub self_ptr: AtomicU64, // 0x00
    /// Kernel stack pointer for `syscall_entry` (GS:0x08).
    pub kernel_rsp: AtomicU64, // 0x08
    /// Saved user stack pointer (GS:0x10).
    pub user_rsp: AtomicU64, // 0x10
    /// Index of the running TCB in the scheduler table (GS:0x18).
    pub current: AtomicU64, // 0x18
    /// Logical CPU index (GS:0x20).
    pub index: AtomicU64, // 0x20
    /// Local APIC id of this CPU.
    pub lapic_id: AtomicU64, // 0x28
    /// Scheduler slot of this CPU's idle thread.
    pub idle_slot: AtomicU64, // 0x30
    /// Set when the CPU has joined the scheduler.
    pub online: AtomicBool,

    /// Task State Segment (ring-0 stack + double-fault IST).
    pub tss: UnsafeCell<TaskStateSegment>,
    /// Per-CPU GDT (loaded before the TSS).
    pub gdt: UnsafeCell<GlobalDescriptorTable>,

    df_stack: AlignedStack<DF_STACK_SIZE>,
    boot_stack: AlignedStack<BOOT_STACK_SIZE>,
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            self_ptr: AtomicU64::new(0),
            kernel_rsp: AtomicU64::new(0),
            user_rsp: AtomicU64::new(0),
            current: AtomicU64::new(0),
            index: AtomicU64::new(0),
            lapic_id: AtomicU64::new(0),
            idle_slot: AtomicU64::new(0),
            online: AtomicBool::new(false),
            tss: UnsafeCell::new(TaskStateSegment::new()),
            gdt: UnsafeCell::new(GlobalDescriptorTable::new()),
            df_stack: AlignedStack([0; DF_STACK_SIZE]),
            boot_stack: AlignedStack([0; BOOT_STACK_SIZE]),
        }
    }

    /// 16-byte-aligned top of this CPU's boot stack.
    pub fn boot_stack_top(&self) -> u64 {
        (self.boot_stack.0.as_ptr() as u64 + BOOT_STACK_SIZE as u64) & !15
    }

    /// 16-byte-aligned top of this CPU's double-fault stack.
    pub fn df_stack_top(&self) -> u64 {
        (self.df_stack.0.as_ptr() as u64 + DF_STACK_SIZE as u64) & !15
    }
}

// WHY unsafe: every slot is owned by exactly one CPU at run time. The
// non-atomic fields are written by the owning CPU (or by the BSP before
// that AP is released); `current`/`kernel_rsp`/`user_rsp` are only ever
// accessed by the owning CPU via the GS segment.
unsafe impl Sync for PerCpu {}

static PER_CPU: [PerCpu; MAX_CPUS] = [const { PerCpu::new() }; MAX_CPUS];

const _: () = assert!(core::mem::offset_of!(PerCpu, kernel_rsp) == 0x08);
const _: () = assert!(core::mem::offset_of!(PerCpu, user_rsp) == 0x10);
const _: () = assert!(core::mem::offset_of!(PerCpu, current) == 0x18);
const _: () = assert!(core::mem::offset_of!(PerCpu, index) == 0x20);

/// Boot stack tops for APs, indexed by slot. Read by `ap_entry` before any
/// Rust code (and therefore before GS is usable) runs on the AP.
#[no_mangle]
pub static AP_STACK_TOPS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

static CPU_COUNT: AtomicU64 = AtomicU64::new(1);

/// Borrow the `PerCpu` slot for a logical CPU index.
pub fn percpu_at(index: usize) -> &'static PerCpu {
    &PER_CPU[index]
}

/// Index of the CPU currently executing this code.
#[inline]
pub fn cpu_index() -> usize {
    let idx: u64;
    // WHY unsafe: reads `PerCpu.index` through the GS base installed by
    // `init_cpu`; GS is kernel-only because CR4.FSGSBASE is left clear.
    unsafe {
        asm!("mov {}, gs:0x20", out(reg) idx,
             options(nomem, nostack, preserves_flags));
    }
    idx as usize
}

/// This CPU's `PerCpu` slot.
pub fn current() -> &'static PerCpu {
    &PER_CPU[cpu_index()]
}

/// Local APIC id of the bootstrap processor.
pub fn bsp_lapic_id() -> u64 {
    PER_CPU[0].lapic_id.load(Ordering::Acquire)
}

/// Number of CPUs Limine reported (used to spread threads at spawn time).
pub fn cpu_count() -> usize {
    CPU_COUNT.load(Ordering::Acquire) as usize
}

/// Number of CPUs that have joined the scheduler.
#[cfg(not(feature = "test"))]
pub fn online_count() -> usize {
    PER_CPU
        .iter()
        .take(MAX_CPUS)
        .filter(|c| c.online.load(Ordering::Acquire))
        .count()
}

/// Bring up per-CPU state on the bootstrap processor.
pub fn init_bsp() {
    let pc = &PER_CPU[0];
    pc.index.store(0, Ordering::Release);
    pc.self_ptr.store(pc as *const _ as u64, Ordering::Release);
    pc.online.store(true, Ordering::Release);
    crate::arch::x86_64::init_cpu(0);
}

/// Read the processor list from Limine and cache the CPU count.
pub fn discover() {
    match crate::bootinfo::MP_REQUEST.response() {
        Some(resp) => {
            let n = resp.cpus().len().clamp(1, MAX_CPUS);
            CPU_COUNT.store(n as u64, Ordering::Release);
            crate::serial_println!("smp: {} cpu(s) reported by limine", n);
        }
        None => {
            CPU_COUNT.store(1, Ordering::Release);
            crate::serial_println!("smp: no MP response, running uniprocessor");
        }
    }
}

/// Start every non-BSP processor and wait until it has joined the scheduler.
#[cfg(not(feature = "test"))]
pub fn start_aps() {
    let Some(resp) = crate::bootinfo::MP_REQUEST.response() else {
        return;
    };

    let mut slot = 1usize;
    for info in resp.cpus() {
        if info.lapic_id as u64 == bsp_lapic_id() {
            continue;
        }
        if slot >= MAX_CPUS {
            break;
        }

        let pc = &PER_CPU[slot];
        pc.index.store(slot as u64, Ordering::Release);
        pc.self_ptr.store(pc as *const _ as u64, Ordering::Release);
        pc.lapic_id.store(info.lapic_id as u64, Ordering::Release);
        AP_STACK_TOPS[slot].store(pc.boot_stack_top(), Ordering::Release);

        crate::serial_println!("smp: starting cpu{} (lapic id={})", slot, info.lapic_id);
        info.bootstrap(ap_entry, slot as u64);

        // APs come up in microseconds; this is only a safety net so a
        // broken AP cannot hang the BSP forever.
        let mut spins = 0u64;
        while !pc.online.load(Ordering::Acquire) && spins < 500_000_000 {
            core::hint::spin_loop();
            spins += 1;
        }

        if pc.online.load(Ordering::Acquire) {
            crate::serial_println!("smp: cpu{} online", slot);
        } else {
            crate::serial_println!("smp: cpu{} failed to start", slot);
        }
        slot += 1;
    }
}

/// First instruction executed on an AP. `rdi` is the `&MpInfo`; the slot
/// index was stored in `extra_argument` (offset 24 in the C layout).
///
/// # Safety
///
/// Called by Limine's `bootstrap` with `extra_argument < MAX_CPUS`;
/// `AP_STACK_TOPS[slot]` must already hold a valid, 16-byte-aligned stack
/// top. Never call this from Rust code.
#[unsafe(naked)]
#[cfg(not(feature = "test"))]
pub unsafe extern "C" fn ap_entry(_info: &MpInfo) -> ! {
    naked_asm!(
        "mov rax, [rdi + 24]",                 // extra_argument = cpu slot
        "lea rdx, [rip + AP_STACK_TOPS]",
        "mov rsp, [rdx + rax*8]",              // own boot stack (16-aligned)
        "mov rdi, rax",                        // pass slot to ap_main
        "xor rbp, rbp",
        "call {main}",
        "ud2",
        main = sym ap_main,
    );
}

#[cfg(not(feature = "test"))]
extern "C" fn ap_main(slot: u64) -> ! {
    let idx = slot as usize;

    crate::arch::x86_64::init_cpu(idx);
    crate::arch::x86_64::load_idt();
    crate::arch::x86_64::enable_lapic_this_cpu();
    crate::arch::x86_64::init_syscalls();

    // WHY unsafe: APs start with the same kernel page tables as the BSP,
    // but reloading CR3 is harmless and pins the invariant explicitly.
    unsafe {
        crate::arch::x86_64::switch_address_space(0, false);
    }

    crate::sched::init_cpu_idle(idx);

    PER_CPU[idx].online.store(true, Ordering::Release);
    crate::serial_println!("ap{}: online", idx);

    x86_64::instructions::interrupts::enable();
    loop {
        crate::sched::schedule();
        // WHY unsafe: idle CPU parks until the next interrupt.
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
