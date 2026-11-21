//! Shared NovaX userspace ABI: syscall numbers and typed wrappers.
//!
//! Servers are compiled standalone by `kernel/heart/build.rs` (plain
//! `rustc`, no cargo), so this file is pulled in with
//! `#[path = "../../common/abi.rs"] mod abi;` rather than being a crate.
//! Each binary uses only a subset of the definitions below.

#![allow(dead_code)] // per-program subsets of the ABI

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_SEND: u64 = 2;
pub const SYS_RECV: u64 = 3;
pub const SYS_YIELD: u64 = 4;
pub const SYS_IRQ_REGISTER: u64 = 5;
pub const SYS_IRQ_ACK: u64 = 6;
pub const SYS_INB: u64 = 7;
pub const SYS_OUTB: u64 = 8;
pub const SYS_DMA_ALLOC: u64 = 9;
pub const SYS_INW: u64 = 10;
pub const SYS_OUTW: u64 = 11;
pub const SYS_INL: u64 = 12;
pub const SYS_OUTL: u64 = 13;
pub const SYS_MAP_MMIO: u64 = 14;

/// Magic number at the start of an `FbInfo` message ("NVFB").
pub const FB_INFO_MAGIC: u32 = 0x4E56_4642;
/// Size of the fixed `FbInfo` wire message.
pub const FB_INFO_LEN: usize = 48;

/// Raw three-argument syscall (number in `rax`, args in `rdi/rsi/rdx`).
///
/// # Safety
///
/// Pointer arguments must stay valid for the whole call. The kernel only
/// range-checks them against the user half; a bogus address faults.
#[inline(always)]
pub unsafe fn syscall3(n: u64, a: u64, b: u64, c: u64) -> u64 {
    let ret: u64;
    // SAFETY: `syscall` is the only ring-3 kernel entry; the caller upholds
    // the per-syscall pointer contract documented above.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            lateout("rcx") _,
            lateout("r11") _,
            lateout("rdi") _,
            lateout("rsi") _,
            lateout("rdx") _,
            options(nostack),
        );
    }
    ret
}

/// Write `buf` to the kernel console (`fd 1`). Returns the byte count.
pub fn write(buf: &[u8]) -> u64 {
    // SAFETY: `buf` is a live slice for the whole call and the kernel only
    // reads `buf.len()` bytes from it.
    unsafe { syscall3(SYS_WRITE, 1, buf.as_ptr() as u64, buf.len() as u64) }
}

/// Send `buf` (at most 64 bytes) through an endpoint capability.
pub fn send(cap: u64, buf: &[u8]) -> u64 {
    // SAFETY: same contract as `write`; the kernel caps the length at 64.
    unsafe { syscall3(SYS_SEND, cap, buf.as_ptr() as u64, buf.len() as u64) }
}

/// Receive up to `buf.len()` bytes from an endpoint capability.
pub fn recv(cap: u64, buf: &mut [u8]) -> u64 {
    // SAFETY: `buf` is exclusively borrowed for the call; the kernel writes
    // at most `buf.len()` bytes into it.
    unsafe { syscall3(SYS_RECV, cap, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

/// Yield the CPU to the scheduler.
pub fn yield_now() {
    // SAFETY: no pointers involved; plain scheduling request.
    unsafe { syscall3(SYS_YIELD, 0, 0, 0) };
}

/// Register `irq_cap` for delivery on `ep_cap`; returns 0 on success.
pub fn irq_register(irq_cap: u64, ep_cap: u64) -> u64 {
    // SAFETY: capability indices are integers; no pointers involved.
    unsafe { syscall3(SYS_IRQ_REGISTER, irq_cap, ep_cap, 0) }
}

/// Re-arm an IRQ line after the driver has drained the device.
pub fn irq_ack(irq_cap: u64) -> u64 {
    // SAFETY: capability index only.
    unsafe { syscall3(SYS_IRQ_ACK, irq_cap, 0, 0) }
}

/// Read one byte from the port window granted by `io_cap`.
pub fn inb(io_cap: u64, offset: u64) -> u64 {
    // SAFETY: the kernel validates `offset` against the capability window.
    unsafe { syscall3(SYS_INB, io_cap, offset, 0) }
}

/// Write one byte into the port window granted by `io_cap`.
pub fn outb(io_cap: u64, offset: u64, value: u64) -> u64 {
    // SAFETY: same capability check as `inb`.
    unsafe { syscall3(SYS_OUTB, io_cap, offset, value) }
}

/// Read one 16-bit word from the port window granted by `io_cap`.
pub fn inw(io_cap: u64, offset: u64) -> u64 {
    // SAFETY: same capability check as `inb`.
    unsafe { syscall3(SYS_INW, io_cap, offset, 0) }
}

/// Write one 16-bit word into the port window granted by `io_cap`.
pub fn outw(io_cap: u64, offset: u64, value: u64) -> u64 {
    // SAFETY: same capability check as `inb`.
    unsafe { syscall3(SYS_OUTW, io_cap, offset, value) }
}

/// Read one 32-bit dword from the port window granted by `io_cap`.
pub fn inl(io_cap: u64, offset: u64) -> u64 {
    // SAFETY: same capability check as `inb`.
    unsafe { syscall3(SYS_INL, io_cap, offset, 0) }
}

/// Write one 32-bit dword into the port window granted by `io_cap`.
pub fn outl(io_cap: u64, offset: u64, value: u64) -> u64 {
    // SAFETY: same capability check as `inb`.
    unsafe { syscall3(SYS_OUTL, io_cap, offset, value) }
}

/// Allocate the DMA window granted by `dma_cap`; the kernel writes
/// `{virt, phys}` (two little-endian `u64`s) into `out`.
pub fn dma_alloc(dma_cap: u64, out: &mut [u8; 16]) -> u64 {
    // SAFETY: `out` is a live 16-byte buffer written exactly once by the
    // kernel before the syscall returns.
    unsafe { syscall3(SYS_DMA_ALLOC, dma_cap, out.as_mut_ptr() as u64, 0) }
}

/// Map the physical window granted by `mmio_cap`; the kernel writes
/// `{virt, paddr, len}` (three little-endian `u64`s) into `out`.
pub fn map_mmio(mmio_cap: u64, out: &mut [u8; 24]) -> u64 {
    // SAFETY: `out` is a live 24-byte buffer written exactly once by the
    // kernel before the syscall returns.
    unsafe { syscall3(SYS_MAP_MMIO, mmio_cap, out.as_mut_ptr() as u64, 0) }
}

/// Terminate the calling thread. Never returns.
pub fn exit(code: u64) -> ! {
    // SAFETY: no pointers; `SYS_EXIT` does not return on success.
    unsafe { syscall3(SYS_EXIT, code, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
