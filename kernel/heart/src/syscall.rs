//! Syscall dispatch and user-memory copy helpers.
//!
//! ABI: `rax` = number, `rdi`/`rsi`/`rdx`/`r10`/`r8` = arguments, result
//! in `rax`. Errors are reported as `u64::MAX` (ENOSYS-style); the kernel
//! internals use `Result` and only convert at this boundary.

use crate::cdt;
use crate::ipc::{
    self, BindSource, CapKind, Capability, DeathReason, Message, PendingCap, CAP_NOTIFY, CAP_READ,
    CAP_SLOT_NONE, CAP_WRITE, MAX_MSG_LEN, NO_NODE,
};
use crate::memory_mut;
use crate::sched;
use novax_memory::PageTableRef;

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
const SYS_MAP_MMIO: u64 = 14;
const SYS_CAP_MINT: u64 = 15;
const SYS_CAP_REVOKE: u64 = 16;
const SYS_SEND_CAP: u64 = 17;
const SYS_RECV_CAP: u64 = 18;
const SYS_CALL: u64 = 19;
const SYS_REPLY: u64 = 20;
const SYS_NOTIFY_SIGNAL: u64 = 21;
const SYS_NOTIFY_WAIT: u64 = 22;
const SYS_NOTIFY_POLL: u64 = 23;
const SYS_NOTIFY_BIND: u64 = 24;
const SYS_SPAWN: u64 = 25;
const SYS_MAP_SHARED: u64 = 26;

const HDR_MAGIC: u32 = ipc::CAP_HEADER_MAGIC;
const HDR_LEN: usize = ipc::CAP_HEADER_LEN;

/// Entry point called from `syscall_entry` with the user arguments.
///
/// Runs with interrupts disabled (FMASK clears IF) and returns the value
/// placed in `rax` for `sysretq`.
#[no_mangle]
pub extern "C" fn syscall_dispatch(num: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
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
        SYS_MAP_MMIO => sys_map_mmio(a1, a2),
        SYS_CAP_MINT => sys_cap_mint(a1, a2, a3),
        SYS_CAP_REVOKE => sys_cap_revoke(a1),
        SYS_SEND_CAP => sys_send_cap(a1, a2, a3),
        SYS_RECV_CAP => sys_recv_cap(a1, a2, a3, a4),
        SYS_CALL => sys_call(a1, a2, a3, a4, a5),
        SYS_REPLY => sys_reply(a1, a2, a3),
        SYS_NOTIFY_SIGNAL => sys_notify_signal(a1, a2),
        SYS_NOTIFY_WAIT => sys_notify_wait(a1),
        SYS_NOTIFY_POLL => sys_notify_poll(a1),
        SYS_NOTIFY_BIND => sys_notify_bind(a1, a2, a3, a4),
        SYS_SPAWN => sys_spawn(a1, a2, a3, a4),
        SYS_MAP_SHARED => sys_map_shared(a1, a2),
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
    if len > MAX_MSG_LEN as u64 {
        return u64::MAX;
    }
    let mut buf = [0u8; MAX_MSG_LEN];
    if copy_from_user(&mut buf[..len as usize], ptr).is_err() {
        return u64::MAX;
    }

    match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & CAP_WRITE != 0 => match c.kind {
            CapKind::Endpoint(id) => {
                let queued = ipc::send(id, Message::plain(buf, len as usize, sched::current_id()));
                if queued {
                    len
                } else {
                    u64::MAX
                }
            }
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

fn sys_recv(cap_idx: u64, ptr: u64, max_len: u64) -> u64 {
    // Validate the destination before dequeuing anything: a bad pointer
    // must not consume (and lose) a message.
    let want = core::cmp::min(max_len, MAX_MSG_LEN as u64);
    if !user_range_ok(ptr, want, true) {
        return u64::MAX;
    }
    match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & CAP_READ != 0 => match c.kind {
            CapKind::Endpoint(id) => loop {
                match ipc::recv(id) {
                    ipc::RecvOutcome::Message(msg) => {
                        // A plain recv cannot hand out capability slots; a
                        // call that lands here is failed so the client is
                        // not left blocked forever.
                        if let Some(reply) = msg.reply {
                            ipc::reply_fail(reply);
                            return u64::MAX;
                        }
                        let n = core::cmp::min(msg.len, max_len as usize);
                        if copy_to_user(ptr, &msg.data[..n]).is_err() {
                            return u64::MAX;
                        }
                        return n as u64;
                    }
                    ipc::RecvOutcome::Blocked => sched::block_current(),
                    ipc::RecvOutcome::Busy | ipc::RecvOutcome::Invalid => return u64::MAX,
                }
            },
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

fn sys_exit(code: i32) -> ! {
    crate::serial_println!("user: exit({})", code);
    ipc::process_died(sched::current_id(), DeathReason::Exit);
    sched::exit_current()
}

// --- Capability lifecycle ---------------------------------------------------

fn ensure_node(cap_idx: u64, cap: Capability) -> Option<u16> {
    cdt::ensure_current_cap_node(cap_idx, cap)
}

fn transfer_kind(kind: CapKind) -> CapKind {
    match kind {
        CapKind::DmaMapped { phys, len } => CapKind::Shm { phys, len },
        other => other,
    }
}

/// Create a reduced child capability in the first free cspace slot.
fn sys_cap_mint(cap_idx: u64, new_rights: u64, out_ptr: u64) -> u64 {
    if !user_range_ok(out_ptr, 8, true) {
        return u64::MAX;
    }
    let cap = match sched::get_current_cap(cap_idx) {
        Some(c) => c,
        None => return u64::MAX,
    };
    let rights = new_rights as u32;
    if rights == 0 || rights & !cap.rights != 0 {
        return u64::MAX;
    }
    let parent = match ensure_node(cap_idx, cap) {
        Some(n) => n,
        None => return u64::MAX,
    };
    let Some(slot) = sched::first_free_cap_slot() else {
        return u64::MAX;
    };
    let kind = transfer_kind(cap.kind);
    let owner = sched::current_id();
    let Some(node) = cdt::alloc_child(parent, owner, slot, kind, rights) else {
        return u64::MAX;
    };
    let child = Capability { kind, rights, node };
    if !sched::install_current_cap_at(slot, child) {
        cdt::free_node(node);
        return u64::MAX;
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(&(slot as u64).to_le_bytes());
    if copy_to_user(out_ptr, &out).is_err() {
        return u64::MAX;
    }
    0
}

fn sys_cap_revoke(cap_idx: u64) -> u64 {
    let cap = match sched::get_current_cap(cap_idx) {
        Some(c) => c,
        None => return u64::MAX,
    };
    if cap.node == NO_NODE || !cdt::exists(cap.node) {
        return 0;
    }
    let removed = cdt::revoke_children(cap.node);
    crate::serial_println!("cap: revoke node={} children={}", cap.node, removed);
    0
}

/// Parse the 12-byte capability header at the start of a message.
fn parse_cap_header(
    buf: &[u8; MAX_MSG_LEN],
    len: usize,
) -> Result<(Option<PendingCap>, usize), ()> {
    if len < HDR_LEN {
        return Err(());
    }
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != HDR_MAGIC {
        return Err(());
    }
    let cap_slot = u16::from_le_bytes([buf[4], buf[5]]);
    let cap_rights = u16::from_le_bytes([buf[6], buf[7]]) as u32;
    let payload = HDR_LEN;
    if cap_slot == CAP_SLOT_NONE {
        return Ok((None, payload));
    }
    let cap = sched::get_current_cap(cap_slot as u64).ok_or(())?;
    let rights = cap.rights & cap_rights;
    if rights == 0 {
        return Err(());
    }
    let parent = ensure_node(cap_slot as u64, cap).ok_or(())?;
    Ok((
        Some(PendingCap {
            parent,
            kind: transfer_kind(cap.kind),
            rights,
        }),
        payload,
    ))
}

fn sys_send_cap(cap_idx: u64, ptr: u64, len: u64) -> u64 {
    if len > MAX_MSG_LEN as u64 {
        return u64::MAX;
    }
    let mut buf = [0u8; MAX_MSG_LEN];
    if copy_from_user(&mut buf[..len as usize], ptr).is_err() {
        return u64::MAX;
    }
    let (pending, payload) = match parse_cap_header(&buf, len as usize) {
        Ok(v) => v,
        Err(()) => return u64::MAX,
    };
    let cap = match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & CAP_WRITE != 0 => c,
        _ => return u64::MAX,
    };
    let id = match cap.kind {
        CapKind::Endpoint(id) => id,
        _ => return u64::MAX,
    };
    let mut data = [0u8; MAX_MSG_LEN];
    let n = len as usize - payload;
    data[..n].copy_from_slice(&buf[payload..len as usize]);
    let mut msg = Message::plain(data, n, sched::current_id());
    msg.cap = pending;
    if ipc::send(id, msg) {
        n as u64
    } else {
        u64::MAX
    }
}

fn install_reply_cap(reply_id: u16) -> Option<i32> {
    let slot = sched::first_free_cap_slot()?;
    let owner = sched::current_id();
    let node = cdt::alloc_root(owner, slot, CapKind::Reply(reply_id), CAP_WRITE)?;
    let cap = Capability {
        kind: CapKind::Reply(reply_id),
        rights: CAP_WRITE,
        node,
    };
    if sched::install_current_cap_at(slot, cap) {
        ipc::reply_attach_server(reply_id, owner);
        Some(slot as i32)
    } else {
        cdt::free_node(node);
        None
    }
}

fn sys_recv_cap(cap_idx: u64, ptr: u64, max_len: u64, meta_ptr: u64) -> u64 {
    let want = core::cmp::min(max_len, MAX_MSG_LEN as u64);
    if !user_range_ok(ptr, want, true) || !user_range_ok(meta_ptr, 8, true) {
        return u64::MAX;
    }
    match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & CAP_READ != 0 => match c.kind {
            CapKind::Endpoint(id) => loop {
                match ipc::recv(id) {
                    ipc::RecvOutcome::Message(msg) => {
                        let mut cap_slot: i32 = -1;
                        let mut reply_slot: i32 = -1;
                        if let Some(pending) = msg.cap {
                            if let Some(slot) = sched::first_free_cap_slot() {
                                let owner = sched::current_id();
                                if let Some(node) = cdt::alloc_child(
                                    pending.parent,
                                    owner,
                                    slot,
                                    pending.kind,
                                    pending.rights,
                                ) {
                                    let cap = Capability {
                                        kind: pending.kind,
                                        rights: pending.rights,
                                        node,
                                    };
                                    if sched::install_current_cap_at(slot, cap) {
                                        cap_slot = slot as i32;
                                    } else {
                                        cdt::free_node(node);
                                    }
                                }
                            }
                        }
                        if let Some(reply) = msg.reply {
                            match install_reply_cap(reply) {
                                Some(slot) => reply_slot = slot,
                                None => ipc::reply_fail(reply),
                            }
                        }
                        let n = core::cmp::min(msg.len, max_len as usize);
                        if copy_to_user(ptr, &msg.data[..n]).is_err() {
                            if let Some(reply) = msg.reply {
                                ipc::reply_fail(reply);
                            }
                            return u64::MAX;
                        }
                        let mut meta = [0u8; 8];
                        meta[0..4].copy_from_slice(&cap_slot.to_le_bytes());
                        meta[4..8].copy_from_slice(&reply_slot.to_le_bytes());
                        if copy_to_user(meta_ptr, &meta).is_err() {
                            if let Some(reply) = msg.reply {
                                ipc::reply_fail(reply);
                            }
                            return u64::MAX;
                        }
                        return n as u64;
                    }
                    ipc::RecvOutcome::Blocked => sched::block_current(),
                    ipc::RecvOutcome::Busy | ipc::RecvOutcome::Invalid => return u64::MAX,
                }
            },
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

fn sys_call(ep_idx: u64, req_ptr: u64, req_len: u64, reply_ptr: u64, reply_max: u64) -> u64 {
    if req_len > MAX_MSG_LEN as u64 {
        return u64::MAX;
    }
    let mut buf = [0u8; MAX_MSG_LEN];
    if copy_from_user(&mut buf[..req_len as usize], req_ptr).is_err() {
        return u64::MAX;
    }
    let (pending, payload) = match parse_cap_header(&buf, req_len as usize) {
        Ok(v) => v,
        Err(()) => return u64::MAX,
    };
    let want = core::cmp::min(reply_max, MAX_MSG_LEN as u64);
    if !user_range_ok(reply_ptr, want, true) {
        return u64::MAX;
    }
    let cap = match sched::get_current_cap(ep_idx) {
        Some(c) if c.rights & CAP_WRITE != 0 => c,
        _ => return u64::MAX,
    };
    let id = match cap.kind {
        CapKind::Endpoint(id) => id,
        _ => return u64::MAX,
    };
    let pml4 = sched::current_pml4_phys();
    let Some(reply) = ipc::alloc_reply(sched::current_id(), pml4, reply_ptr, want) else {
        return u64::MAX;
    };
    let mut data = [0u8; MAX_MSG_LEN];
    let n = req_len as usize - payload;
    data[..n].copy_from_slice(&buf[payload..req_len as usize]);
    let mut msg = Message::plain(data, n, sched::current_id());
    msg.cap = pending;
    msg.reply = Some(reply);
    if !ipc::send(id, msg) {
        ipc::reply_abort(reply);
        return u64::MAX;
    }
    match ipc::reply_wait(reply) {
        Some(n) => n,
        None => u64::MAX,
    }
}

fn sys_reply(cap_idx: u64, ptr: u64, len: u64) -> u64 {
    if len > MAX_MSG_LEN as u64 {
        return u64::MAX;
    }
    let cap = match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & CAP_WRITE != 0 => c,
        _ => return u64::MAX,
    };
    let reply = match cap.kind {
        CapKind::Reply(id) => id,
        _ => return u64::MAX,
    };
    let mut buf = [0u8; MAX_MSG_LEN];
    if copy_from_user(&mut buf[..len as usize], ptr).is_err() {
        return u64::MAX;
    }
    let me = sched::current_id();
    let result = if ipc::reply_is_owned_by(reply, me) {
        ipc::reply_complete(reply, &buf[..len as usize])
    } else {
        Err(())
    };
    // A reply capability is one-shot: burn it even if delivery failed.
    sched::clear_current_cap(cap_idx);
    if cap.node != NO_NODE {
        cdt::free_node(cap.node);
    }
    match result {
        Ok(n) => n,
        Err(()) => u64::MAX,
    }
}

// --- Notifications ----------------------------------------------------------

fn notify_cap(cap_idx: u64, rights: u32) -> Option<u16> {
    match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & rights == rights => match c.kind {
            CapKind::Notification(nid) => Some(nid),
            _ => None,
        },
        _ => None,
    }
}

fn sys_notify_signal(cap_idx: u64, badge: u64) -> u64 {
    let Some(nid) = notify_cap(cap_idx, CAP_NOTIFY) else {
        return u64::MAX;
    };
    if ipc::notify_signal(nid, badge) {
        0
    } else {
        u64::MAX
    }
}

fn sys_notify_wait(cap_idx: u64) -> u64 {
    let Some(nid) = notify_cap(cap_idx, CAP_READ | CAP_NOTIFY) else {
        return u64::MAX;
    };
    ipc::notify_wait(nid).unwrap_or(u64::MAX)
}

fn sys_notify_poll(cap_idx: u64) -> u64 {
    let Some(nid) = notify_cap(cap_idx, CAP_READ) else {
        return u64::MAX;
    };
    ipc::notify_poll(nid).unwrap_or(u64::MAX)
}

fn sys_notify_bind(cap_idx: u64, source: u64, src_cap_idx: u64, badge: u64) -> u64 {
    let Some(nid) = notify_cap(cap_idx, CAP_NOTIFY) else {
        return u64::MAX;
    };
    let src = match sched::get_current_cap(src_cap_idx) {
        Some(c) => c,
        None => return u64::MAX,
    };
    match source {
        0 => match src.kind {
            CapKind::Irq(irq) if src.rights & CAP_NOTIFY != 0 => {
                if ipc::bind_irq_notification(irq, nid, badge) {
                    0
                } else {
                    u64::MAX
                }
            }
            _ => u64::MAX,
        },
        1 | 2 => match src.kind {
            CapKind::Process(tid) => {
                let source = if source == 1 {
                    BindSource::Exit
                } else {
                    BindSource::Fault
                };
                if ipc::bind_process_notification(nid, tid, badge, source) {
                    0
                } else {
                    u64::MAX
                }
            }
            _ => u64::MAX,
        },
        _ => u64::MAX,
    }
}

// --- Spawn ------------------------------------------------------------------

fn sys_spawn(prog_id: u64, grants_ptr: u64, grants_len: u64, out_ptr: u64) -> u64 {
    if grants_len > 64 || grants_len % 8 != 0 || !user_range_ok(out_ptr, 8, true) {
        return u64::MAX;
    }
    let mut raw = [0u8; 64];
    if copy_from_user(&mut raw[..grants_len as usize], grants_ptr).is_err() {
        return u64::MAX;
    }
    let mut grants = [crate::user::CapGrant {
        src_slot: 0,
        rights: 0,
    }; 8];
    let count = grants_len as usize / 8;
    for i in 0..count {
        grants[i].src_slot =
            u32::from_le_bytes([raw[i * 8], raw[i * 8 + 1], raw[i * 8 + 2], raw[i * 8 + 3]]);
        grants[i].rights = u32::from_le_bytes([
            raw[i * 8 + 4],
            raw[i * 8 + 5],
            raw[i * 8 + 6],
            raw[i * 8 + 7],
        ]);
    }
    match crate::user::spawn_program(prog_id, &grants[..count]) {
        Ok(slot) => {
            let mut meta = [0u8; 8];
            meta[0..4].copy_from_slice(&(slot as i32).to_le_bytes());
            if copy_to_user(out_ptr, &meta).is_err() {
                return u64::MAX;
            }
            0
        }
        Err(e) => {
            crate::serial_println!("spawn: failed: {:?}", e);
            u64::MAX
        }
    }
}

fn sys_map_shared(cap_idx: u64, out_ptr: u64) -> u64 {
    let cap = match sched::get_current_cap(cap_idx) {
        Some(c) if c.rights & CAP_READ != 0 => c,
        _ => return u64::MAX,
    };
    match crate::shm::map_cap(cap, out_ptr) {
        Ok(()) => 0,
        Err(e) => {
            crate::serial_println!("shm: map failed: {:?}", e);
            u64::MAX
        }
    }
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
/// the whole access (`width` bytes) stays inside the capability window.
fn io_port(cap_idx: u64, offset: u64, right: u32, width: u64) -> Option<u16> {
    let c = sched::get_current_cap(cap_idx)?;
    if c.rights & right == 0 {
        return None;
    }
    match c.kind {
        CapKind::IoPort { base, len } => {
            let end = offset.checked_add(width)?;
            if end > len as u64 {
                return None;
            }
            base.checked_add(offset as u16)
        }
        _ => None,
    }
}

fn sys_inb(cap_idx: u64, offset: u64) -> u64 {
    match io_port(cap_idx, offset, CAP_READ, 1) {
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
    match io_port(cap_idx, offset, CAP_WRITE, 1) {
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
    match io_port(cap_idx, offset, CAP_READ, 2) {
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
    match io_port(cap_idx, offset, CAP_WRITE, 2) {
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
    match io_port(cap_idx, offset, CAP_READ, 4) {
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
    match io_port(cap_idx, offset, CAP_WRITE, 4) {
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
    let len = match cap.kind {
        CapKind::Dma { len } | CapKind::DmaMapped { len, .. } => len,
        _ => return u64::MAX,
    };
    match crate::dma::alloc_for_current(len, out_ptr) {
        Ok((_, phys)) => {
            sched::set_current_cap_kind(cap_idx, CapKind::DmaMapped { phys, len });
            0
        }
        Err(e) => {
            crate::serial_println!("dma: alloc failed: {:?}", e);
            u64::MAX
        }
    }
}

/// Map a physical MMIO window granted by `CapKind::Mmio` into the caller
/// and report `{virt, paddr, len}`. The kernel never reads or writes the
/// window itself.
fn sys_map_mmio(cap_idx: u64, out_ptr: u64) -> u64 {
    let cap = match sched::get_current_cap(cap_idx) {
        Some(c) => c,
        None => return u64::MAX,
    };
    if cap.rights & (CAP_READ | CAP_WRITE) != CAP_READ | CAP_WRITE {
        return u64::MAX;
    }
    match cap.kind {
        CapKind::Mmio { paddr, len } => match crate::mmio::map_for_current(paddr, len, out_ptr) {
            Ok(()) => 0,
            Err(e) => {
                crate::serial_println!("mmio: map failed: {:?}", e);
                u64::MAX
            }
        },
        _ => u64::MAX,
    }
}

/// Check that `[ptr, ptr + len)` is mapped for ring 3 in the current
/// address space with the requested write permission.
///
/// This is the syscall-boundary guard: a user pointer is only dereferenced
/// after every page it touches has been walked and its `USER`/`WRITABLE`
/// bits checked, so an unmapped or supervisor-only address returns an
/// error instead of faulting in ring 0.
pub fn user_range_ok(ptr: u64, len: u64, write: bool) -> bool {
    let pml4 = sched::current_pml4_phys();
    if pml4 == 0 {
        return false;
    }
    user_range_ok_in(pml4, ptr, len, write)
}

/// [`user_range_ok`] for an arbitrary address space.
pub fn user_range_ok_in(pml4: u64, ptr: u64, len: u64, write: bool) -> bool {
    const USER_MAX: u64 = 0x0000_8000_0000_0000;
    if len == 0 {
        return true;
    }
    let Some(end) = ptr.checked_add(len) else {
        return false;
    };
    if ptr >= USER_MAX || end > USER_MAX {
        return false;
    }
    let hhdm = memory_mut(|m| m.frames.hhdm());
    PageTableRef::from_phys(pml4, hhdm).check_user_range(ptr, len, write)
}

/// Copy `dst.len()` bytes from the user address `src`.
///
/// The range is checked against the current address space before the copy;
/// a pointer to an unmapped page is rejected. The kernel still has no SMAP,
/// so a concurrent unmap would be a kernel fault, but NovaX has no unmap
/// syscall and pins each process to one CPU.
pub fn copy_from_user(dst: &mut [u8], src: u64) -> Result<(), ()> {
    if !user_range_ok(src, dst.len() as u64, false) {
        return Err(());
    }
    for (i, b) in dst.iter_mut().enumerate() {
        // SAFETY: the range was checked against the current PML4 above; the
        // kernel has no SMAP, so a user page is readable from ring 0.
        unsafe { *b = core::ptr::read_volatile((src + i as u64) as *const u8) };
    }
    Ok(())
}

/// Copy `src` to the user address `dst`.
///
/// Same contract as [`copy_from_user`], plus a `WRITABLE` check on every
/// page.
pub fn copy_to_user(dst: u64, src: &[u8]) -> Result<(), ()> {
    if !user_range_ok(dst, src.len() as u64, true) {
        return Err(());
    }
    for (i, b) in src.iter().enumerate() {
        // SAFETY: same contract as `copy_from_user`: the range is mapped
        // writable for ring 3, one byte per access.
        unsafe {
            core::ptr::write_volatile((dst + i as u64) as *mut u8, *b);
        }
    }
    Ok(())
}

/// Copy `src` into the address space `pml4`, translating each byte through
/// that process's page tables. Used by `SYS_REPLY` while the client is
/// blocked and a different address space is active.
pub fn copy_to_user_in(pml4: u64, dst: u64, src: &[u8]) -> Result<(), ()> {
    if pml4 == 0 || !user_range_ok_in(pml4, dst, src.len() as u64, true) {
        return Err(());
    }
    let hhdm = memory_mut(|m| m.frames.hhdm());
    let paging = PageTableRef::from_phys(pml4, hhdm);
    for (i, b) in src.iter().enumerate() {
        let va = dst + i as u64;
        let phys = paging.translate(va).ok_or(())?;
        // SAFETY: `phys` came from a present user PTE in `pml4`; the HHDM
        // maps every physical frame for the lifetime of the kernel.
        unsafe {
            core::ptr::write_volatile((hhdm.as_u64() + phys) as *mut u8, *b);
        }
    }
    Ok(())
}
