//! Syscall dispatch and user-memory copy helpers.
//!
//! ABI: `rax` = number, `rdi`/`rsi`/`rdx` = arguments, result in `rax`.
//! Errors are reported as `u64::MAX` (ENOSYS-style); the kernel internals
//! use `Result` and only convert at this boundary.

use crate::ipc::{self, CapKind, Message, CAP_NOTIFY, CAP_READ, CAP_WRITE};
use crate::sched;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_SEND: u64 = 2;
const SYS_RECV: u64 = 3;
const SYS_YIELD: u64 = 4;
const SYS_IRQ_REGISTER: u64 = 5;
const SYS_IRQ_ACK: u64 = 6;
const SYS_INB: u64 = 7;
const SYS_OUTB: u64 = 8;
const SYS_DMA_ALLOC: u64 = 9;
const SYS_INW: u64 = 10;
const SYS_OUTW: u64 = 11;
const SYS_INL: u64 = 12;
const SYS_OUTL: u64 = 13;

/// Entry point called from `syscall_entry` with the user arguments.
///
/// Runs with interrupts disabled (FMASK clears IF) and returns the value
/// placed in `rax` for `sysretq`.
#[no_mangle]
pub extern "C" fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    match num {
        SYS_EXIT => sys_exit(a1 as i32),
        SYS_WRITE => sys_write(a1, a2, a3),
        SYS_SEND => sys_send(a1, a2, a3),
        SYS_RECV => sys_recv(a1, a2, a3),
        SYS_YIELD => {
            sched::yield_now();
            0
        }
        SYS_IRQ_REGISTER => sys_irq_register(a1, a2),
        SYS_IRQ_ACK => sys_irq_ack(a1),
        SYS_INB => sys_inb(a1, a2),
        SYS_OUTB => sys_outb(a1, a2, a3),
        SYS_DMA_ALLOC => sys_dma_alloc(a1, a2),
        SYS_INW => sys_inw(a1, a2),
        SYS_OUTW => sys_outw(a1, a2, a3),
        SYS_INL => sys_inl(a1, a2),
        SYS_OUTL => sys_outl(a1, a2, a3),
        _ => u64::MAX, // ENOSYS
    }
}

fn sys_write(fd: u64, ptr: u64, len: u64) -> u64 {
    if fd != 1 {
        return u64::MAX;
    }
    const MAX: u64 = 4096;
    let n = if len > MAX { MAX } else { len };

    let mut buf = [0u8; MAX as usize];
    if copy_from_user(&mut buf[..n as usize], ptr).is_err() {
        return u64::MAX;
    }

    let s = core::str::from_utf8(&buf[..n as usize]).unwrap_or("<binary>");
    crate::serial_print!("{}", s);
    n
}

fn sys_send(cap_idx: u64, ptr: u64, len: u64) -> u64 {
    if len > 64 {
        return u64::MAX;
    }
    let mut buf = [0u8; 64];
    if copy_from_user(&mut buf[..len as usize], ptr).is_err() {
        return u64::MAX;
    }

    match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & CAP_WRITE != 0 => match c.kind {
            CapKind::Endpoint(id) => {
                ipc::send(
                    id,
                    Message {
                        data: buf,
                        len: len as usize,
                        sender: sched::current_id(),
                    },
                );
                len
            }
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

fn sys_recv(cap_idx: u64, ptr: u64, max_len: u64) -> u64 {
    match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & CAP_READ != 0 => match c.kind {
            CapKind::Endpoint(id) => loop {
                if let Some(msg) = ipc::recv(id) {
                    let n = core::cmp::min(msg.len, max_len as usize);
                    if copy_to_user(ptr, &msg.data[..n]).is_err() {
                        return u64::MAX;
                    }
                    return n as u64;
                }
                sched::block_current();
            },
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

fn sys_exit(code: i32) -> ! {
    crate::serial_println!("user: exit({})", code);
    sched::exit_current()
}

// --- Userspace driver syscalls ----------------------------------------------

/// Bind `irq_cap` (CapKind::Irq) to `ep_cap` (CapKind::Endpoint). The
/// kernel will enqueue a one-byte message on that endpoint whenever the
/// IRQ fires.
fn sys_irq_register(irq_idx: u64, ep_idx: u64) -> u64 {
    let irq_cap = match sched::get_current_cap(irq_idx) {
        Some(c) => c,
        None => return u64::MAX,
    };
    let ep_cap = match sched::get_current_cap(ep_idx) {
        Some(c) => c,
        None => return u64::MAX,
    };
    if irq_cap.rights & CAP_NOTIFY == 0 {
        return u64::MAX;
    }
    if ep_cap.rights & CAP_WRITE == 0 {
        return u64::MAX;
    }

    let irq = match irq_cap.kind {
        CapKind::Irq(n) => n,
        _ => return u64::MAX,
    };
    let ep = match ep_cap.kind {
        CapKind::Endpoint(id) => id,
        _ => return u64::MAX,
    };

    if ipc::route_irq(irq, ep) {
        0
    } else {
        u64::MAX
    }
}

/// Re-arm the line after the driver has drained the device. The LAPIC
/// EOI itself was already issued by the interrupt handler.
fn sys_irq_ack(irq_idx: u64) -> u64 {
    match sched::get_current_cap(irq_idx) {
        Some(c) if c.rights & CAP_NOTIFY != 0 => match c.kind {
            CapKind::Irq(irq) => {
                crate::arch::x86_64::irq_ack(irq);
                0
            }
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

/// Resolve an (io-cap, offset) pair to a physical port, enforcing that
/// the offset stays inside the capability's window.
fn io_port(cap_idx: u64, offset: u64, right: u32) -> Option<u16> {
    let c = sched::get_current_cap(cap_idx)?;
    if c.rights & right == 0 {
        return None;
    }
    match c.kind {
        CapKind::IoPort { base, len } => {
            if offset >= len as u64 {
                return None;
            }
            base.checked_add(offset as u16)
        }
        _ => None,
    }
}

fn sys_inb(cap_idx: u64, offset: u64) -> u64 {
    match io_port(cap_idx, offset, CAP_READ) {
        Some(port) => {
            // WHY unsafe: port I/O is privileged; the capability has
            // already bounded `port` to the granted window.
            let v: u8;
            unsafe {
                core::arch::asm!("in al, dx",
                                 in("dx") port,
                                 out("al") v,
                                 options(nomem, nostack, preserves_flags));
            }
            v as u64
        }
        None => u64::MAX,
    }
}

fn sys_outb(cap_idx: u64, offset: u64, value: u64) -> u64 {
    match io_port(cap_idx, offset, CAP_WRITE) {
        Some(port) => {
            // WHY unsafe: same contract as sys_inb.
            unsafe {
                core::arch::asm!("out dx, al",
                                 in("dx") port,
                                 in("al") value as u8,
                                 options(nomem, nostack, preserves_flags));
            }
            0
        }
        None => u64::MAX,
    }
}

fn sys_inw(cap_idx: u64, offset: u64) -> u64 {
    match io_port(cap_idx, offset, CAP_READ) {
        Some(port) => {
            // WHY unsafe: privileged port I/O inside the granted window.
            let v: u16;
            unsafe {
                core::arch::asm!("in ax, dx",
                                 in("dx") port,
                                 out("ax") v,
                                 options(nomem, nostack, preserves_flags));
            }
            v as u64
        }
        None => u64::MAX,
    }
}

fn sys_outw(cap_idx: u64, offset: u64, value: u64) -> u64 {
    match io_port(cap_idx, offset, CAP_WRITE) {
        Some(port) => {
            // WHY unsafe: same contract as `sys_inw`.
            unsafe {
                core::arch::asm!("out dx, ax",
                                 in("dx") port,
                                 in("ax") value as u16,
                                 options(nomem, nostack, preserves_flags));
            }
            0
        }
        None => u64::MAX,
    }
}

fn sys_inl(cap_idx: u64, offset: u64) -> u64 {
    match io_port(cap_idx, offset, CAP_READ) {
        Some(port) => {
            // WHY unsafe: privileged port I/O inside the granted window.
            let v: u32;
            unsafe {
                core::arch::asm!("in eax, dx",
                                 in("dx") port,
                                 out("eax") v,
                                 options(nomem, nostack, preserves_flags));
            }
            v as u64
        }
        None => u64::MAX,
    }
}

fn sys_outl(cap_idx: u64, offset: u64, value: u64) -> u64 {
    match io_port(cap_idx, offset, CAP_WRITE) {
        Some(port) => {
            // WHY unsafe: same contract as `sys_inl`.
            unsafe {
                core::arch::asm!("out dx, eax",
                                 in("dx") port,
                                 in("eax") value as u32,
                                 options(nomem, nostack, preserves_flags));
            }
            0
        }
        None => u64::MAX,
    }
}

/// Map a contiguous DMA window into the caller and report `{virt, phys}`.
/// Requires a `CapKind::Dma` capability; the kernel never hands out more
/// than the capability's `len`.
fn sys_dma_alloc(cap_idx: u64, out_ptr: u64) -> u64 {
    let cap = match sched::get_current_cap(cap_idx) {
        Some(c) => c,
        None => return u64::MAX,
    };
    if cap.rights & (CAP_READ | CAP_WRITE) != CAP_READ | CAP_WRITE {
        return u64::MAX;
    }
    match cap.kind {
        CapKind::Dma { len } => match crate::dma::alloc_for_current(len, out_ptr) {
            Ok(()) => 0,
            Err(e) => {
                crate::serial_println!("dma: alloc failed: {:?}", e);
                u64::MAX
            }
        },
        _ => u64::MAX,
    }
}

/// Copy `dst.len()` bytes from the user address `src`.
///
/// Only range-checks against the user half; the kernel has no SMAP, so a
/// bad pointer is a kernel fault (documented v0.1.0 limitation).
pub fn copy_from_user(dst: &mut [u8], src: u64) -> Result<(), ()> {
    const USER_MAX: u64 = 0x0000_8000_0000_0000;
    let end = src.checked_add(dst.len() as u64).ok_or(())?;
    if src >= USER_MAX || end > USER_MAX {
        return Err(());
    }
    for (i, b) in dst.iter_mut().enumerate() {
        // SAFETY: the range was bounds-checked against USER_MAX above; the
        // kernel has no SMAP, so a user page is readable from ring 0. A bad
        // pointer is still a kernel fault (see the v0.1.0 limitations).
        unsafe { *b = core::ptr::read_volatile((src + i as u64) as *const u8) };
    }
    Ok(())
}

/// Copy `src` to the user address `dst`.
///
/// Same range-check contract as [`copy_from_user`].
pub fn copy_to_user(dst: u64, src: &[u8]) -> Result<(), ()> {
    const USER_MAX: u64 = 0x0000_8000_0000_0000;
    let end = dst.checked_add(src.len() as u64).ok_or(())?;
    if dst >= USER_MAX || end > USER_MAX {
        return Err(());
    }
    for (i, b) in src.iter().enumerate() {
        // SAFETY: same contract as `copy_from_user`: range checked, one byte
        // per access, user pages are writable without SMAP.
        unsafe {
            core::ptr::write_volatile((dst + i as u64) as *mut u8, *b);
        }
    }
    Ok(())
}
