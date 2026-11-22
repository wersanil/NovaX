//! x86_64 architecture support: GDT, TSS, IDT, PIC mask, LAPIC timer,
//! syscall entry, user-mode transitions.

use core::arch::{asm, global_asm, naked_asm};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use spin::Once;
use x86_64::instructions::segmentation::{Segment, CS, DS, ES, SS};
use x86_64::instructions::tables::load_tss;
use x86_64::registers::control::Cr3;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::PrivilegeLevel;
use x86_64::VirtAddr;

use crate::arch::smp;
use crate::{memory_mut, serial_println};
use novax_memory::{NO_EXEC, PCD, PRESENT, PWT, WRITABLE};

// --- Constants --------------------------------------------------------------

const LAPIC_PHYS_BASE: u64 = 0xFEE0_0000;
const LAPIC_VIRT_BASE: u64 = 0xffff_ffff_b000_0000;

const IOAPIC_PHYS_BASE: u64 = 0xFEC0_0000;
const IOAPIC_VIRT_BASE: u64 = 0xffff_ffff_b100_0000;

const LAPIC_REG_ID: usize = 0x020;
const LAPIC_REG_EOI: usize = 0x0B0;
const LAPIC_REG_SVR: usize = 0x0F0;
const LAPIC_REG_ICR_LOW: usize = 0x300;
const LAPIC_REG_ICR_HIGH: usize = 0x310;
const LAPIC_REG_LVT_TIMER: usize = 0x320;
const LAPIC_REG_TIMER_INIT: usize = 0x380;
#[allow(dead_code)]
const LAPIC_REG_TIMER_CUR: usize = 0x390;
const LAPIC_REG_TIMER_DIV: usize = 0x3E0;

const IOAPIC_REG_ID: u32 = 0x00;
const IOAPIC_REG_VER: u32 = 0x01;
const IOAPIC_REG_TABLE: u32 = 0x10;

const IA32_GS_BASE: u32 = 0xC000_0101;

const TIMER_VECTOR: u8 = 32;
/// Inter-processor reschedule IPI.
pub const RESCHED_VECTOR: u8 = 0x30;
/// Legacy PIC IRQ1 (PS/2 keyboard) after the 8259 remap to 0x20..0x2F.
const KBD_VECTOR: u8 = 33;
/// Legacy PIC IRQ4 (COM1) after the remap.
const SERIAL_VECTOR: u8 = 36;
const LAPIC_DIV_16: u32 = 0b0011;
const LAPIC_TIMER_INITIAL: u32 = 10_000_000;

// --- Globals ----------------------------------------------------------------

static IDT: Once<InterruptDescriptorTable> = Once::new();

/// Address of the mapped LAPIC MMIO page (0 until `init_lapic`).
pub static LAPIC_VIRT: AtomicU64 = AtomicU64::new(0);
/// Address of the mapped IOAPIC MMIO page (0 until `init_ioapic`).
pub static IOAPIC_VIRT: AtomicU64 = AtomicU64::new(0);
/// LAPIC timer tick counter.
pub static TICKS: AtomicU32 = AtomicU32::new(0);

static KERNEL_PML4_PHYS: AtomicU64 = AtomicU64::new(0);

// --- Public API -------------------------------------------------------------

/// Bring up this CPU's GDT/TSS/IDT, mask the legacy PIC, map the LAPIC,
/// program the IOAPIC, and configure MSRs for `syscall`.
pub fn init() {
    // Establish our own GDT/CS first: `set_handler_fn` records the CS
    // selector that is current when the IDT is built.
    smp::init_bsp();
    init_idt();
    load_idt();
    init_pic();
    // The LAPIC must be mapped and its id published before the IOAPIC
    // entries are steered to this CPU.
    init_lapic();
    init_ioapic();
    init_syscalls();
    smp::discover();
}

// --- Per-CPU GDT + TSS ------------------------------------------------------

/// Build and load the GDT/TSS for `index`, load the shared IDT, and
/// install the per-CPU pointer in GS. Safe to call once per CPU.
pub fn init_cpu(index: usize) {
    let pc = smp::percpu_at(index);
    pc.index.store(index as u64, Ordering::Release);
    pc.self_ptr.store(pc as *const _ as u64, Ordering::Release);

    // TSS: ring-0 stack for interrupts/syscalls and an IST stack for
    // double faults, both private to this CPU.
    // WHY unsafe: exclusive access to this CPU's own TssCell.
    unsafe {
        let tss = &mut *pc.tss.get();
        tss.privilege_stack_table[0] = VirtAddr::new(pc.boot_stack_top());
        tss.interrupt_stack_table[0] = VirtAddr::new(pc.df_stack_top());
    }

    // WHY unsafe: exclusive access to this CPU's own GdtCell; the
    // descriptor table is built before the first load and never mutated.
    unsafe {
        let gdt = &mut *pc.gdt.get();
        *gdt = GlobalDescriptorTable::new();
        gdt.append(Descriptor::kernel_code_segment()); // 1: 0x08
        gdt.append(Descriptor::kernel_data_segment()); // 2: 0x10
        gdt.append(Descriptor::user_data_segment()); // 3: 0x18 -> sel 0x1B
        gdt.append(Descriptor::user_code_segment()); // 4: 0x20 -> sel 0x23
        let tss_ref: &'static TaskStateSegment = &*pc.tss.get();
        gdt.append(Descriptor::tss_segment(tss_ref)); // 5: 0x28
        gdt.load();
    }

    let code_sel = SegmentSelector::new(1, PrivilegeLevel::Ring0);
    let data_sel = SegmentSelector::new(2, PrivilegeLevel::Ring0);
    let tss_sel = SegmentSelector::new(5, PrivilegeLevel::Ring0);

    // WHY unsafe: `load_tss` is privileged; called once per CPU before
    // its interrupts are unmasked.
    unsafe {
        load_tss(tss_sel);
    }

    set_data_segments(data_sel);
    set_code_segment(code_sel);

    // WHY unsafe: GS base is a privileged MSR. Ring 3 cannot read it
    // because CR4.FSGSBASE is not enabled.
    unsafe {
        wrmsr(IA32_GS_BASE, pc as *const _ as u64);
    }
}

/// Load the IDT built by `init_idt` (or by `init` on the BSP).
pub fn load_idt() {
    IDT.get().expect("idt: init_idt first").load();
}

fn set_data_segments(data_sel: SegmentSelector) {
    // WHY unsafe: segment register writes are privileged; ring 0 only.
    unsafe {
        DS::set_reg(data_sel);
        ES::set_reg(data_sel);
        SS::set_reg(data_sel);
    }
}

fn set_code_segment(code_sel: SegmentSelector) {
    // WHY unsafe: CS write via far return; called before IRQs are enabled.
    unsafe {
        CS::set_reg(code_sel);
    }
}

// --- IDT --------------------------------------------------------------------

fn init_idt() {
    let idt = IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();

        idt.breakpoint.set_handler_fn(breakpoint_handler);
        idt.divide_error.set_handler_fn(divide_error_handler);
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
        idt.stack_segment_fault
            .set_handler_fn(stack_segment_fault_handler);
        idt.general_protection_fault
            .set_handler_fn(general_protection_handler);
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.alignment_check.set_handler_fn(alignment_check_handler);

        // WHY unsafe: `set_stack_index` is unsafe because a bad index
        // would let the CPU load a garbage rsp on a fault. Index 1 is
        // IST1, which we populated in TSS.interrupt_stack_table[0].
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault_handler)
                .set_stack_index(1);
        }

        idt[TIMER_VECTOR].set_handler_fn(timer_handler);
        idt[RESCHED_VECTOR].set_handler_fn(resched_handler);
        idt[KBD_VECTOR].set_handler_fn(keyboard_irq_handler);
        idt[SERIAL_VECTOR].set_handler_fn(serial_irq_handler);

        idt
    });
    idt.load();
}

// --- PIC --------------------------------------------------------------------

/// Remap the 8259 pair to vectors 0x20..0x2F and mask every line.
/// Individual IRQs are unmasked on demand by the capability holder
/// (`SYS_IRQ_ACK`, see `ipc::irq_ack`).
fn init_pic() {
    // WHY unsafe: raw port I/O to the PIC; ring 0 only, before any IRQ
    // is unmasked. ICW sequence follows the Intel 8259 datasheet.
    unsafe {
        outb(0x20, 0x11);
        io_wait();
        outb(0xA0, 0x11);
        io_wait();
        outb(0x21, 0x20);
        io_wait(); // master vector base = 32
        outb(0xA1, 0x28);
        io_wait(); // slave vector base = 40
        outb(0x21, 0x04);
        io_wait();
        outb(0xA1, 0x02);
        io_wait();
        outb(0x21, 0x01);
        io_wait();
        outb(0xA1, 0x01);
        io_wait();
        outb(0x21, 0xFF);
        outb(0xA1, 0xFF);
    }
}

#[inline]
fn io_wait() {
    // WHY unsafe: port 0x80 is the traditional POST delay port.
    unsafe {
        outb(0x80, 0);
    }
}

// --- IOAPIC ----------------------------------------------------------------

/// Map the IOAPIC MMIO page, mask every redirection entry and record the
/// device so `irq_mask`/`irq_unmask` can program individual lines.
fn init_ioapic() {
    let flags = PRESENT | WRITABLE | NO_EXEC | PCD | PWT;
    memory_mut(|m| {
        m.paging
            .map(IOAPIC_VIRT_BASE, IOAPIC_PHYS_BASE, flags, &mut m.frames)
            .expect("IOAPIC MMIO map");
    });
    IOAPIC_VIRT.store(IOAPIC_VIRT_BASE, Ordering::Release);

    let version = ioapic_read(IOAPIC_REG_VER);
    let max_entry = ((version >> 16) & 0xFF) as u8;

    // `init_lapic` runs first and publishes the real BSP LAPIC id; using
    // the default 0 here would steer every IRQ to a non-existent CPU on
    // hardware whose BSP is not CPU 0.
    let bsp_lapic = smp::bsp_lapic_id() as u8;

    for gsi in 0..=max_entry {
        ioapic_write_entry(gsi, 0, bsp_lapic, true);
    }
    // Pre-program the two lines the userspace drivers own, still masked.
    ioapic_write_entry(1, KBD_VECTOR, bsp_lapic, true);
    ioapic_write_entry(4, SERIAL_VECTOR, bsp_lapic, true);

    serial_println!(
        "ioapic: id={}, version={:#x}, entries={}",
        ioapic_read(IOAPIC_REG_ID) >> 24,
        version & 0xFF,
        max_entry as u32 + 1
    );
}

fn ioapic_read(reg: u32) -> u32 {
    let base = IOAPIC_VIRT.load(Ordering::Acquire) as *mut u8;
    // WHY unsafe: MMIO access to the IOAPIC window mapped above.
    unsafe {
        core::ptr::write_volatile(base as *mut u32, reg);
        core::ptr::read_volatile(base.add(0x10) as *const u32)
    }
}

fn ioapic_write(reg: u32, value: u32) {
    let base = IOAPIC_VIRT.load(Ordering::Acquire) as *mut u8;
    // WHY unsafe: same window as `ioapic_read`.
    unsafe {
        core::ptr::write_volatile(base as *mut u32, reg);
        core::ptr::write_volatile(base.add(0x10) as *mut u32, value);
    }
}

/// Program one redirection entry. Physical destination mode, active-high
/// edge trigger (the ISA defaults).
fn ioapic_write_entry(gsi: u8, vector: u8, dest_lapic: u8, masked: bool) {
    let low_reg = IOAPIC_REG_TABLE + (gsi as u32) * 2;
    let mut low = vector as u32;
    if masked {
        low |= 1 << 16;
    }
    ioapic_write(low_reg, low);
    ioapic_write(low_reg + 1, (dest_lapic as u32) << 24);
}

fn ioapic_set_mask(irq: u8, masked: bool) {
    let low_reg = IOAPIC_REG_TABLE + (irq as u32) * 2;
    let mut low = ioapic_read(low_reg);
    if masked {
        low |= 1 << 16;
    } else {
        low &= !(1 << 16);
    }
    ioapic_write(low_reg, low);
}

/// Mask one IRQ line so the handler is not re-entered before the
/// userspace driver has serviced the device.
pub fn irq_mask(irq: u8) {
    ioapic_set_mask(irq, true);
}

/// Re-arm an IRQ line after the userspace driver drained the device.
pub fn irq_unmask(irq: u8) {
    ioapic_set_mask(irq, false);
}

/// Signal end-of-interrupt to this CPU's LAPIC.
pub fn lapic_eoi() {
    let base = LAPIC_VIRT.load(Ordering::Acquire) as *mut u8;
    if !base.is_null() {
        // WHY unsafe: MMIO write to our own LAPIC EOI register.
        unsafe {
            write_reg(base, LAPIC_REG_EOI, 0);
        }
    }
}

/// Driver finished draining the device: re-arm the line. The EOI was
/// already sent from the interrupt handler.
pub fn irq_ack(irq: u8) {
    irq_unmask(irq);
}

/// Send a fixed IPI to one logical CPU.
pub fn send_ipi(dest_lapic: u8, vector: u8) {
    let base = LAPIC_VIRT.load(Ordering::Acquire) as *mut u8;
    if base.is_null() {
        return;
    }
    // WHY unsafe: MMIO writes to the local APIC's ICR; base validated at init.
    unsafe {
        write_reg(base, LAPIC_REG_ICR_HIGH, (dest_lapic as u32) << 24);
        write_reg(base, LAPIC_REG_ICR_LOW, 0x4000 | vector as u32);
    }
}

/// Raw byte write to a port. Caller owns the port (ring 0 only).
#[inline]
pub(crate) unsafe fn outb(port: u16, val: u8) {
    // WHY unsafe: privileged instruction.
    unsafe {
        asm!("out dx, al",
             in("dx") port,
             in("al") val,
             options(nomem, nostack, preserves_flags));
    }
}

// --- LAPIC ------------------------------------------------------------------

fn init_lapic() {
    // limine's HHDM does not cover MMIO; map the LAPIC page ourselves.
    // Intel requires the LAPIC page to be uncacheable.
    let flags = PRESENT | WRITABLE | NO_EXEC | PCD | PWT;
    memory_mut(|m| {
        m.paging
            .map(LAPIC_VIRT_BASE, LAPIC_PHYS_BASE, flags, &mut m.frames)
            .expect("LAPIC MMIO map");
    });

    LAPIC_VIRT.store(LAPIC_VIRT_BASE, Ordering::Release);

    // Publish the BSP's APIC id so `init_ioapic` (which runs next) can
    // steer every redirection entry to this CPU.
    let mmio = LAPIC_VIRT_BASE as *mut u8;
    // WHY unsafe: reading our own LAPIC ID register after mapping.
    let id = unsafe { read_reg(mmio, LAPIC_REG_ID) >> 24 };
    smp::percpu_at(0)
        .lapic_id
        .store(id as u64, Ordering::Release);
    serial_println!("lapic: bsp id={}, virt={:#x}", id, LAPIC_VIRT_BASE);

    enable_lapic_this_cpu();
}

/// Enable the LAPIC and arm its timer on the current CPU. Called by the
/// BSP during `init_lapic` and by each AP after `init_cpu`.
pub fn enable_lapic_this_cpu() {
    let mmio = LAPIC_VIRT.load(Ordering::Acquire) as *mut u8;
    if mmio.is_null() {
        return;
    }

    // WHY unsafe: MMIO writes to LAPIC registers via our own mapping;
    // offsets are 16-byte-aligned, accesses are 32-bit per Intel SDM.
    unsafe {
        let mut apic_base = rdmsr(0x1B);
        apic_base |= 1 << 11;
        wrmsr(0x1B, apic_base);

        // Spurious-vector 0xFF, APIC software-enable bit 8.
        write_reg(mmio, LAPIC_REG_SVR, 0xFF | (1 << 8));
        write_reg(mmio, LAPIC_REG_TIMER_DIV, LAPIC_DIV_16);
        write_reg(mmio, LAPIC_REG_LVT_TIMER, TIMER_VECTOR as u32 | (1 << 17));
        write_reg(mmio, LAPIC_REG_TIMER_INIT, LAPIC_TIMER_INITIAL);
    }
}

/// # Safety
///
/// `mmio` must point at the mapped LAPIC page and `offset` must be a
/// 16-byte-aligned register offset inside it.
#[inline]
unsafe fn write_reg(mmio: *mut u8, offset: usize, val: u32) {
    // WHY unsafe: MMIO write; caller guarantees offset within LAPIC window.
    unsafe {
        core::ptr::write_volatile(mmio.add(offset) as *mut u32, val);
    }
}

/// # Safety
///
/// Same contract as [`write_reg`].
#[inline]
unsafe fn read_reg(mmio: *mut u8, offset: usize) -> u32 {
    // WHY unsafe: MMIO read; same reasoning as write_reg.
    unsafe { core::ptr::read_volatile(mmio.add(offset) as *const u32) }
}

/// # Safety
///
/// Privileged instruction; must be executed in ring 0 with a valid MSR.
unsafe fn rdmsr(msr: u32) -> u64 {
    // WHY unsafe: privileged instruction, ring 0 only.
    let (lo, hi): (u32, u32);
    unsafe {
        asm!("rdmsr",
             in("ecx") msr,
             out("eax") lo,
             out("edx") hi,
             options(nomem, nostack, preserves_flags));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// # Safety
///
/// Privileged instruction; must be executed in ring 0 with a valid MSR.
unsafe fn wrmsr(msr: u32, val: u64) {
    // WHY unsafe: privileged instruction, ring 0 only.
    unsafe {
        asm!("wrmsr",
             in("ecx") msr,
             in("eax") val as u32,
             in("edx") (val >> 32) as u32,
             options(nomem, nostack, preserves_flags));
    }
}

// --- Interrupt handlers -----------------------------------------------------

extern "x86-interrupt" fn breakpoint_handler(frame: InterruptStackFrame) {
    serial_println!(
        "[int3] breakpoint at {:#x}",
        frame.instruction_pointer.as_u64()
    );
}

/// True when the interrupted context was ring 3.
fn from_user(frame: &InterruptStackFrame) -> bool {
    frame.code_segment.rpl() == PrivilegeLevel::Ring3
}

/// A ring-3 exception must not take the kernel down: terminate the faulty
/// thread and let the scheduler pick another one. Faults from ring 0 still
/// panic, because they indicate a kernel bug.
fn kill_user_thread(what: &str, frame: &InterruptStackFrame) -> ! {
    serial_println!(
        "user fault: {} at rip={:#x} rsp={:#x}",
        what,
        frame.instruction_pointer.as_u64(),
        frame.stack_pointer.as_u64()
    );
    crate::sched::exit_current()
}

extern "x86-interrupt" fn divide_error_handler(frame: InterruptStackFrame) {
    if from_user(&frame) {
        kill_user_thread("divide error", &frame);
    }
    panic!("divide error at {:#x}", frame.instruction_pointer.as_u64());
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    if from_user(&frame) {
        kill_user_thread("invalid opcode", &frame);
    }
    panic!(
        "invalid opcode at {:#x}",
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn stack_segment_fault_handler(frame: InterruptStackFrame, error_code: u64) {
    if from_user(&frame) {
        kill_user_thread("stack segment fault", &frame);
    }
    panic!(
        "stack segment fault: error={:#x} rip={:#x}",
        error_code,
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn general_protection_handler(frame: InterruptStackFrame, error_code: u64) {
    if from_user(&frame) {
        kill_user_thread("general protection", &frame);
    }
    panic!(
        "general protection fault: error={:#x} rip={:#x}",
        error_code,
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let cr2: u64;
    // WHY unsafe: reading CR2 is privileged; we're in ring 0 in a handler.
    unsafe {
        asm!("mov {}, cr2", out(reg) cr2, options(nostack, preserves_flags));
    }
    if from_user(&frame) {
        serial_println!(
            "user page fault: addr={:#x} err={:?} rip={:#x}",
            cr2,
            error_code,
            frame.instruction_pointer.as_u64()
        );
        crate::sched::exit_current();
    }
    panic!(
        "page fault: addr={:#x} err={:?} rip={:#x}",
        cr2,
        error_code,
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn alignment_check_handler(frame: InterruptStackFrame, error_code: u64) {
    if from_user(&frame) {
        kill_user_thread("alignment check", &frame);
    }
    panic!(
        "alignment check: error={:#x} rip={:#x}",
        error_code,
        frame.instruction_pointer.as_u64()
    );
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, _error_code: u64) -> ! {
    serial_println!("*** DOUBLE FAULT ***");
    serial_println!(
        "rip={:#x} rsp={:#x}",
        frame.instruction_pointer.as_u64(),
        frame.stack_pointer.as_u64()
    );
    crate::halt_forever()
}

extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    lapic_eoi();

    if n <= 10 || n % 100 == 0 {
        serial_println!("tick #{}", n);
    }

    crate::sched::schedule();
}

/// Reschedule IPI: another CPU published work that may belong to us.
extern "x86-interrupt" fn resched_handler(_frame: InterruptStackFrame) {
    lapic_eoi();
    crate::sched::schedule();
}

// --- Userspace driver IRQs --------------------------------------------------

/// Delivery model: mask the line, push a one-byte message (the IRQ
/// number) to the endpoint registered for that IRQ, then let the driver
/// EOI + unmask via `SYS_IRQ_ACK` once it has drained the device.
extern "x86-interrupt" fn keyboard_irq_handler(_frame: InterruptStackFrame) {
    irq_mask(1);
    crate::ipc::deliver_irq(1);
    // The in-service bit must be cleared here: LAPIC blocks every vector
    // at or below the one in service, which would also starve the timer.
    lapic_eoi();
}

extern "x86-interrupt" fn serial_irq_handler(_frame: InterruptStackFrame) {
    irq_mask(4);
    crate::ipc::deliver_irq(4);
    lapic_eoi();
}

// --- Context switch ---------------------------------------------------------

/// Save callee-saved registers + RFLAGS on the current stack, store RSP
/// into `*old_rsp`, load `new_rsp`, restore, and `ret`.
///
/// # Safety
///
/// - `old_rsp` must point to a writable `u64`.
/// - `new_rsp` must point to a frame with the same layout.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(_old_rsp: *mut u64, _new_rsp: u64) {
    naked_asm!(
        "pushfq",
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "popfq",
        "ret",
    );
}

// --- Syscalls ---------------------------------------------------------------

global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    // Per-CPU slots live in the GS-relative `PerCpu` (offsets checked in
    // arch::smp): 0x08 = kernel_rsp, 0x10 = user_rsp.
    "    mov qword ptr gs:0x10, rsp",
    "    mov rsp, qword ptr gs:0x08",
    "    push qword ptr gs:0x10", // save user rsp
    "    push rcx",               // save user rip
    "    push r11",               // save user rflags
    //
    // Syscall ABI: every GPR except rax (return), rcx and r11 (clobbered by
    // syscall/sysret itself) must survive the call. rdi/rsi/rdx restored
    // below; r8/r9/r10 are used as scratch by Rust code, so save them too.
    "    push rdi",
    "    push rsi",
    "    push rdx",
    "    push r8",
    "    push r9",
    "    push r10",
    //
    // 9 pushes leave rsp ≡ 8 (mod 16); pad to keep SysV 16-byte alignment
    // at the callee entry point.
    "    sub rsp, 8",
    //
    // Подготовка аргументов для syscall_dispatch(num, a1, a2, a3)
    "    mov rcx, rdx", // a3 -> rcx
    "    mov rdx, rsi", // a2 -> rdx
    "    mov rsi, rdi", // a1 -> rsi
    "    mov rdi, rax", // num -> rdi
    "    call syscall_dispatch",
    //
    "    add rsp, 8",
    "    pop r10",
    "    pop r9",
    "    pop r8",
    "    pop rdx",
    "    pop rsi",
    "    pop rdi",
    "    pop r11",
    "    pop rcx",
    // Restore user rsp straight from the frame's last slot: no scratch
    // register needed, so no GPR is clobbered on return.
    "    mov rsp, [rsp]",
    "    sysretq",
);

extern "C" {
    fn syscall_entry();
}

/// Remember the boot PML4 before any user address space is loaded.
pub fn capture_kernel_pml4() {
    let (frame, _) = Cr3::read();
    KERNEL_PML4_PHYS.store(frame.start_address().as_u64(), Ordering::Release);
}

/// Physical address of the kernel PML4 captured at boot.
#[allow(dead_code)] // используется только из user.rs (non-test)
pub fn kernel_pml4_phys() -> u64 {
    KERNEL_PML4_PHYS.load(Ordering::Acquire)
}

/// Load `pml4_phys` into CR3 if `to_user`, else reload the kernel PML4.
///
/// # Safety
///
/// Must be called with IF=0 and a valid, page-aligned physical PML4.
pub unsafe fn switch_address_space(pml4_phys: u64, to_user: bool) {
    let target = if to_user {
        pml4_phys
    } else {
        KERNEL_PML4_PHYS.load(Ordering::Acquire)
    };
    if target == 0 {
        return;
    }
    // WHY unsafe: privileged CR3 write; caller satisfies the contract.
    unsafe {
        let frame = x86_64::structures::paging::PhysFrame::containing_address(
            x86_64::PhysAddr::new(target),
        );
        Cr3::write(frame, x86_64::registers::control::Cr3Flags::empty());
    }
}

/// Update this CPU's TSS.rsp0 and the GS-relative kernel stack slot.
pub fn set_kernel_stack(rsp: u64) {
    let pc = smp::current();
    pc.kernel_rsp.store(rsp, Ordering::Release);
    // WHY unsafe: this CPU's TSS is touched only by the owning CPU while
    // interrupts are disabled (the scheduler holds IF=0 across the switch).
    unsafe {
        (*pc.tss.get()).privilege_stack_table[0] = VirtAddr::new(rsp);
    }
}

/// Program EFER/STAR/LSTAR/FMASK so `syscall`/`sysretq` enter the kernel
/// through `syscall_entry` with interrupts disabled.
pub fn init_syscalls() {
    capture_kernel_pml4();

    // EFER.SCE = 1 enables syscall/sysret.
    // WHY unsafe: WRMSR is privileged.
    unsafe {
        let efer = rdmsr(0xC000_0080);
        wrmsr(0xC000_0080, efer | 1);
    }

    // STAR: [47:32]=KERNEL_CS (0x08), [63:48]=USER_DS-8 (0x13).
    // SYSRET: CS=0x23, SS=0x1B.
    let star: u64 = (0x13u64 << 48) | (0x08u64 << 32);
    // WHY unsafe: WRMSR is privileged.
    unsafe {
        wrmsr(0xC000_0081, star);
    }

    // LSTAR: syscall entry point.
    // WHY unsafe: WRMSR is privileged.
    unsafe {
        wrmsr(0xC000_0082, syscall_entry as usize as u64);
    }

    // FMASK: clear IF, TF, DF on syscall entry.
    // WHY unsafe: WRMSR is privileged.
    unsafe {
        wrmsr(0xC000_0084, 0x700);
    }
}

// --- Entering user mode -----------------------------------------------------

/// First switch into ring 3. Never returns.
///
/// # Safety
///
/// - `entry` must point at mapped user code.
/// - `stack` must point at mapped, writable user stack.
/// - Current CR3 must be the target thread's PML4.
pub unsafe fn enter_userspace(entry: u64, stack: u64) -> ! {
    // WHY unsafe: `iretq` to ring 3 is the only way out of ring 0
    // without a syscall. All values are validated by the caller.
    unsafe {
        asm!(
            "push {user_ss}",
            "push {stack}",
            "push {rflags}",
            "push {user_cs}",
            "push {entry}",
            "iretq",
            user_ss = in(reg) 0x1Bu64,
            user_cs = in(reg) 0x23u64,
            rflags  = in(reg) 0x202u64,
            stack   = in(reg) stack,
            entry   = in(reg) entry,
            options(noreturn),
        );
    }
}
