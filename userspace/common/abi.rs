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
pub const SYS_CAP_MINT: u64 = 15;
pub const SYS_CAP_REVOKE: u64 = 16;
pub const SYS_SEND_CAP: u64 = 17;
pub const SYS_RECV_CAP: u64 = 18;
pub const SYS_CALL: u64 = 19;
pub const SYS_REPLY: u64 = 20;
pub const SYS_NOTIFY_SIGNAL: u64 = 21;
pub const SYS_NOTIFY_WAIT: u64 = 22;
pub const SYS_NOTIFY_POLL: u64 = 23;
pub const SYS_NOTIFY_BIND: u64 = 24;
pub const SYS_SPAWN: u64 = 25;
pub const SYS_MAP_SHARED: u64 = 26;

/// Capability rights (mirror of the kernel constants).
pub const CAP_READ: u32 = 1;
pub const CAP_WRITE: u32 = 2;
pub const CAP_NOTIFY: u32 = 4;

/// `IpcHeader.cap_slot` value meaning "no capability attached".
pub const CAP_SLOT_NONE: u16 = u16::MAX;
/// Magic at the start of an [`IpcHeader`] ("NVCH").
pub const CAP_HEADER_MAGIC: u32 = 0x4E56_4348;
/// Size of [`IpcHeader`].
pub const CAP_HEADER_LEN: usize = 12;

/// Bind source constants for `SYS_NOTIFY_BIND`.
pub const NOTIFY_SRC_IRQ: u64 = 0;
pub const NOTIFY_SRC_EXIT: u64 = 1;
pub const NOTIFY_SRC_FAULT: u64 = 2;

/// Program ids accepted by `SYS_SPAWN`.
pub const PROG_V02_SERVER: u64 = 1;
pub const PROG_V02_CLIENT: u64 = 2;

/// Header that prefixes a capability-transferring message.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IpcHeader {
    pub magic: u32,
    pub cap_slot: u16,
    pub cap_rights: u16,
    pub flags: u32,
}

impl IpcHeader {
    pub const fn new(cap_slot: u16, cap_rights: u16) -> Self {
        Self {
            magic: CAP_HEADER_MAGIC,
            cap_slot,
            cap_rights,
            flags: 0,
        }
    }

    pub fn write_to(self, buf: &mut [u8]) {
        buf[0..4].copy_from_slice(&self.magic.to_le_bytes());
        buf[4..6].copy_from_slice(&self.cap_slot.to_le_bytes());
        buf[6..8].copy_from_slice(&self.cap_rights.to_le_bytes());
        buf[8..12].copy_from_slice(&self.flags.to_le_bytes());
    }
}

/// Metadata returned by `SYS_RECV_CAP`; `-1` means "none".
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RecvMeta {
    pub cap_slot: i32,
    pub reply_slot: i32,
}

impl RecvMeta {
    pub const fn empty() -> Self {
        Self {
            cap_slot: -1,
            reply_slot: -1,
        }
    }
}

/// One capability granted to a child by `SYS_SPAWN`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CapGrant {
    pub src_slot: u32,
    pub rights: u32,
}

/// Result of `SYS_SPAWN`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SpawnMeta {
    pub proc_slot: i32,
    pub reserved: u32,
}

/// Result of `SYS_MAP_SHARED`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct MapInfo {
    pub virt: u64,
    pub phys: u64,
    pub len: u64,
}

/// Raw four-argument syscall (fourth argument in `r10`).
#[inline(always)]
pub unsafe fn syscall4(n: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    let ret: u64;
    // SAFETY: caller upholds the per-syscall pointer contract.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            lateout("rcx") _,
            lateout("r11") _,
            lateout("rdi") _,
            lateout("rsi") _,
            lateout("rdx") _,
            lateout("r10") _,
            options(nostack),
        );
    }
    ret
}

/// Raw five-argument syscall (fourth in `r10`, fifth in `r8`).
#[inline(always)]
pub unsafe fn syscall5(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
    let ret: u64;
    // SAFETY: caller upholds the per-syscall pointer contract.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") n => ret,
            in("rdi") a,
            in("rsi") b,
            in("rdx") c,
            in("r10") d,
            in("r8") e,
            lateout("rcx") _,
            lateout("r11") _,
            lateout("rdi") _,
            lateout("rsi") _,
            lateout("rdx") _,
            lateout("r10") _,
            lateout("r8") _,
            options(nostack),
        );
    }
    ret
}

/// Mint a reduced child capability; writes the new slot to `out`.
pub fn cap_mint(cap: u64, rights: u32, out: &mut u64) -> u64 {
    // SAFETY: `out` is a live u64 written exactly once.
    unsafe { syscall3(SYS_CAP_MINT, cap, rights as u64, out as *mut u64 as u64) }
}

/// Revoke every descendant of `cap` (the root stays in place).
pub fn cap_revoke(cap: u64) -> u64 {
    // SAFETY: capability index only.
    unsafe { syscall3(SYS_CAP_REVOKE, cap, 0, 0) }
}

/// Send a message carrying exactly one capability.
pub fn send_cap(ep: u64, cap_slot: u16, cap_rights: u16, payload: &[u8]) -> u64 {
    let mut buf = [0u8; 64];
    IpcHeader::new(cap_slot, cap_rights).write_to(&mut buf);
    let n = core::cmp::min(payload.len(), 64 - CAP_HEADER_LEN);
    buf[CAP_HEADER_LEN..CAP_HEADER_LEN + n].copy_from_slice(&payload[..n]);
    // SAFETY: `buf` is a live 64-byte buffer for the whole call.
    unsafe {
        syscall3(
            SYS_SEND_CAP,
            ep,
            buf.as_ptr() as u64,
            (CAP_HEADER_LEN + n) as u64,
        )
    }
}

/// Receive a message and learn which cspace slots received the transferred
/// capability and the one-shot reply capability.
pub fn recv_cap(ep: u64, buf: &mut [u8], meta: &mut RecvMeta) -> u64 {
    // SAFETY: both buffers are live for the whole call.
    unsafe {
        syscall4(
            SYS_RECV_CAP,
            ep,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            meta as *mut RecvMeta as u64,
        )
    }
}

/// Synchronous request/response. The request starts with an [`IpcHeader`].
pub fn call(ep: u64, cap_slot: u16, cap_rights: u16, payload: &[u8], reply: &mut [u8]) -> u64 {
    let mut buf = [0u8; 64];
    IpcHeader::new(cap_slot, cap_rights).write_to(&mut buf);
    let n = core::cmp::min(payload.len(), 64 - CAP_HEADER_LEN);
    buf[CAP_HEADER_LEN..CAP_HEADER_LEN + n].copy_from_slice(&payload[..n]);
    // SAFETY: request and reply buffers are live for the whole call.
    unsafe {
        syscall5(
            SYS_CALL,
            ep,
            buf.as_ptr() as u64,
            (CAP_HEADER_LEN + n) as u64,
            reply.as_mut_ptr() as u64,
            reply.len() as u64,
        )
    }
}

/// Consume a one-shot reply capability.
pub fn reply(cap: u64, payload: &[u8]) -> u64 {
    // SAFETY: `payload` is a live slice.
    unsafe {
        syscall3(
            SYS_REPLY,
            cap,
            payload.as_ptr() as u64,
            payload.len() as u64,
        )
    }
}

/// Atomically OR `badge` into the notification mask.
pub fn notify_signal(cap: u64, badge: u64) -> u64 {
    // SAFETY: capability index and integer badge only.
    unsafe { syscall3(SYS_NOTIFY_SIGNAL, cap, badge, 0) }
}

/// Block until an event bit is pending; returns the consumed mask.
pub fn notify_wait(cap: u64) -> u64 {
    // SAFETY: capability index only.
    unsafe { syscall3(SYS_NOTIFY_WAIT, cap, 0, 0) }
}

/// Non-blocking `swap(0)` of the notification mask.
pub fn notify_poll(cap: u64) -> u64 {
    // SAFETY: capability index only.
    unsafe { syscall3(SYS_NOTIFY_POLL, cap, 0, 0) }
}

/// Bind an IRQ or child-process exit/fault to a notification.
pub fn notify_bind(notify: u64, source: u64, src_cap: u64, badge: u64) -> u64 {
    // SAFETY: capability indices and integers only.
    unsafe { syscall4(SYS_NOTIFY_BIND, notify, source, src_cap, badge) }
}

/// Spawn an embedded program, deriving its capabilities from `grants`.
pub fn spawn(prog_id: u64, grants: &[CapGrant], meta: &mut SpawnMeta) -> u64 {
    // SAFETY: `grants` and `meta` are live for the whole call.
    unsafe {
        syscall4(
            SYS_SPAWN,
            prog_id,
            grants.as_ptr() as u64,
            (grants.len() * core::mem::size_of::<CapGrant>()) as u64,
            meta as *mut SpawnMeta as u64,
        )
    }
}

/// Map a read-only shared buffer.
pub fn map_shared(cap: u64, info: &mut MapInfo) -> u64 {
    // SAFETY: `info` is a live 24-byte buffer.
    unsafe { syscall3(SYS_MAP_SHARED, cap, info as *mut MapInfo as u64, 0) }
}

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
