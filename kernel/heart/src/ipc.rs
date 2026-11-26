//! Kernel IPC: endpoints, fixed-size messages, capabilities and IRQ routes.
//!
//! An endpoint has a bounded FIFO (messages ≤ [`MAX_MSG_LEN`]) and at most
//! one blocked receiver. IRQ delivery is expressed as a one-byte message
//! on an endpoint bound with [`route_irq`], so drivers never need a
//! kernel-side callback.
//!
//! v0.2 adds capability transfer, one-shot reply capabilities and
//! asynchronous notifications. The capability derivation tree lives in
//! [`crate::cdt`]; this module only carries the pending descriptor of a
//! capability until the receiver installs it.

use crate::sched::ThreadId;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use spin::Mutex;

/// Sentinel for "this capability is not tracked in the CDT".
pub const NO_NODE: u16 = u16::MAX;

/// A capability held in a thread's cspace (16 slots per thread).
#[derive(Clone, Copy, Debug)]
pub struct Capability {
    /// What the capability grants access to.
    pub kind: CapKind,
    /// Bit mask of `CAP_*` rights required by the operation.
    pub rights: u32,
    /// CDT node that tracks this capability, or [`NO_NODE`].
    pub node: u16,
}

/// The object a [`Capability`] refers to.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "test", allow(dead_code))] // built by the non-test boot path
pub enum CapKind {
    /// Message endpoint; the id indexes [`IpcManager::endpoints`].
    Endpoint(u64),
    /// Legacy ISA IRQ line the holder may register and acknowledge.
    Irq(u8),
    /// Bounded port-I/O window.
    IoPort {
        /// First port of the window.
        base: u16,
        /// Window length in bytes.
        len: u16,
    },
    /// Upper bound on the DMA window this thread may map.
    Dma {
        /// Maximum number of bytes the holder may map.
        len: u32,
    },
    /// Physical MMIO window the holder may map into its own address space.
    Mmio {
        /// Physical base address.
        paddr: u64,
        /// Window length in bytes.
        len: u64,
    },
    /// DMA window that has been allocated and mapped in its owner.
    DmaMapped {
        /// Physical base of the contiguous buffer.
        phys: u64,
        /// Allocated length in bytes.
        len: u32,
    },
    /// Read-only view of a shared buffer, derived from a [`CapKind::DmaMapped`].
    Shm {
        /// Physical base of the shared buffer.
        phys: u64,
        /// Length in bytes.
        len: u32,
    },
    /// Asynchronous event notification object.
    Notification(u16),
    /// One-shot reply capability created by `SYS_CALL`.
    Reply(u16),
    /// Handle on a child process (does not expose its global thread id).
    Process(ThreadId),
}

/// Right to receive from an endpoint.
pub const CAP_READ: u32 = 1;
/// Right to send to an endpoint or write an I/O port.
pub const CAP_WRITE: u32 = 2;
/// Right to register an IRQ capability for kernel-to-userspace delivery.
pub const CAP_NOTIFY: u32 = 4;

/// Maximum payload of one IPC message.
pub const MAX_MSG_LEN: usize = 64;
/// Maximum number of queued messages per endpoint. Both the blocking and
/// the best-effort senders respect this bound so a ring-3 flood cannot
/// exhaust the kernel heap.
pub const MAX_QUEUE: usize = 1024;
/// Maximum number of queued messages for non-blocking senders.
pub const MAX_TRY_QUEUE: usize = MAX_QUEUE;
/// Number of legacy ISA IRQ lines the router tracks.
pub const MAX_IRQS: usize = 16;
/// Size of the `IpcHeader` that prefixes a capability-transferring message.
pub const CAP_HEADER_LEN: usize = 12;
/// Magic at the start of [`CAP_HEADER_LEN`] ("NVCH").
pub const CAP_HEADER_MAGIC: u32 = 0x4E56_4348;
/// `cap_slot` value meaning "this message carries no capability".
pub const CAP_SLOT_NONE: u16 = u16::MAX;

/// Maximum number of notification objects.
pub const MAX_NOTIFICATIONS: usize = 32;
/// Maximum number of waiters queued on one notification.
pub const MAX_NOTIFY_WAITERS: usize = 8;
/// Maximum number of process/exit/fault bindings.
pub const MAX_PROC_BINDINGS: usize = 32;
/// Maximum number of in-flight `SYS_CALL` replies.
pub const MAX_REPLIES: usize = 16;

/// Descriptor of a capability travelling with a message. The CDT child is
/// created only when the receiver installs it, so a sender that dies while
/// its message is still queued cannot leave a dangling node behind.
#[derive(Clone, Copy, Debug)]
pub struct PendingCap {
    /// CDT node of the sender's capability.
    pub parent: u16,
    /// Object kind being transferred.
    pub kind: CapKind,
    /// Rights after truncation.
    pub rights: u32,
}

/// One queued IPC message. `len` bytes of `data` are meaningful.
#[derive(Clone, Copy, Debug)]
pub struct Message {
    /// Payload bytes.
    pub data: [u8; MAX_MSG_LEN],
    /// Number of valid bytes in `data`.
    pub len: usize,
    /// Thread that sent the message (0 for kernel-originated IRQ notes).
    #[allow(dead_code)]
    pub sender: ThreadId,
    /// Capability transferred with this message, if any.
    pub cap: Option<PendingCap>,
    /// Reply slot created by `SYS_CALL`, consumed by `SYS_REPLY`.
    pub reply: Option<u16>,
}

impl Message {
    /// Build a plain message without capability or reply metadata.
    pub fn plain(data: [u8; MAX_MSG_LEN], len: usize, sender: ThreadId) -> Self {
        Self {
            data,
            len,
            sender,
            cap: None,
            reply: None,
        }
    }
}

/// A message endpoint with a FIFO and at most one blocked receiver.
pub struct Endpoint {
    /// Stable endpoint id, also used by capabilities.
    pub id: u64,
    /// Pending messages, oldest first.
    pub queue: VecDeque<Message>,
    /// Thread waiting in `recv`, woken by the next `send`.
    pub blocked_receiver: Option<ThreadId>,
}

/// Global endpoint table.
pub struct IpcManager {
    /// All endpoints created at boot; ids are indices into this vector.
    pub endpoints: Vec<Endpoint>,
}

/// Global IPC state. All access goes through this mutex.
pub static IPC: Mutex<IpcManager> = Mutex::new(IpcManager {
    endpoints: Vec::new(),
});

/// Create a new endpoint and return its id.
#[cfg_attr(feature = "test", allow(dead_code))]
pub fn create_endpoint() -> u64 {
    let mut ipc = IPC.lock();
    let id = ipc.endpoints.len() as u64;
    ipc.endpoints.push(Endpoint {
        id,
        queue: VecDeque::new(),
        blocked_receiver: None,
    });
    id
}

/// Queue `msg` on `ep_id` and wake a blocked receiver, if any.
///
/// Returns `false` when the endpoint does not exist or its FIFO is full;
/// the message is then dropped and the caller reports an error. This bound
/// is what keeps a flooding ring-3 sender from growing the queue without
/// limit.
pub fn send(ep_id: u64, msg: Message) -> bool {
    let mut ipc = IPC.lock();
    let Some(ep) = ipc.endpoints.iter_mut().find(|e| e.id == ep_id) else {
        return false;
    };
    if ep.queue.len() >= MAX_QUEUE {
        return false;
    }
    ep.queue.push_back(msg);
    let wake = ep.blocked_receiver.take();
    drop(ipc);
    if let Some(tid) = wake {
        crate::sched::wake_thread(tid);
    }
    true
}

/// Best-effort variant of [`send`] for kernel console mirroring.
///
/// Returns `false` instead of waiting when the endpoint lock is busy or the
/// receiver's queue is full. The caller (the serial console) must never
/// block on a slow or stalled userspace consumer.
pub fn try_send(ep_id: u64, msg: Message) -> bool {
    let Some(mut ipc) = IPC.try_lock() else {
        return false;
    };
    let Some(ep) = ipc.endpoints.iter_mut().find(|e| e.id == ep_id) else {
        return false;
    };
    if ep.queue.len() >= MAX_TRY_QUEUE {
        return false;
    }
    ep.queue.push_back(msg);
    let wake = ep.blocked_receiver.take();
    drop(ipc);
    if let Some(tid) = wake {
        crate::sched::wake_thread(tid);
    }
    true
}

/// Outcome of a non-blocking receive attempt.
#[derive(Clone, Copy, Debug)]
pub enum RecvOutcome {
    /// A message was dequeued.
    Message(Message),
    /// The caller is now the endpoint's blocked receiver.
    Blocked,
    /// Another thread is already blocked on this endpoint. NovaX supports
    /// exactly one blocked receiver, so the caller gets an error instead of
    /// overwriting (and losing) the existing waiter.
    Busy,
    /// No endpoint with this id exists.
    Invalid,
}

/// Pop one message from `ep_id`, or register the caller as the blocked
/// receiver. Returns [`RecvOutcome::Blocked`] in the latter case (the
/// caller then blocks).
pub fn recv(ep_id: u64) -> RecvOutcome {
    let mut ipc = IPC.lock();
    let Some(ep) = ipc.endpoints.iter_mut().find(|e| e.id == ep_id) else {
        return RecvOutcome::Invalid;
    };
    if let Some(msg) = ep.queue.pop_front() {
        return RecvOutcome::Message(msg);
    }
    let me = crate::sched::current_id();
    match ep.blocked_receiver {
        None => {
            ep.blocked_receiver = Some(me);
            RecvOutcome::Blocked
        }
        Some(tid) if tid == me => RecvOutcome::Blocked,
        Some(_) => RecvOutcome::Busy,
    }
}

// --- Notifications ----------------------------------------------------------

/// One asynchronous notification object: an atomic event mask plus a small
/// fixed wait queue.
pub struct Notification {
    allocated: AtomicBool,
    bits: AtomicU64,
    waiters: Mutex<[u64; MAX_NOTIFY_WAITERS]>,
}

impl Notification {
    const fn new() -> Self {
        Self {
            allocated: AtomicBool::new(false),
            bits: AtomicU64::new(0),
            waiters: Mutex::new([0; MAX_NOTIFY_WAITERS]),
        }
    }
}

static NOTIFICATIONS: [Notification; MAX_NOTIFICATIONS] =
    [const { Notification::new() }; MAX_NOTIFICATIONS];

/// Allocate a notification object. Returns `None` when the pool is full.
pub fn create_notification() -> Option<u16> {
    for (i, n) in NOTIFICATIONS.iter().enumerate() {
        if n.allocated
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            n.bits.store(0, Ordering::Release);
            return Some(i as u16);
        }
    }
    None
}

fn notification(nid: u16) -> Option<&'static Notification> {
    NOTIFICATIONS
        .get(nid as usize)
        .filter(|n| n.allocated.load(Ordering::Acquire))
}

/// Atomically OR `badge` into the notification mask and wake every waiter.
pub fn notify_signal(nid: u16, badge: u64) -> bool {
    let Some(n) = notification(nid) else {
        return false;
    };
    n.bits.fetch_or(badge, Ordering::AcqRel);
    let woken: [u64; MAX_NOTIFY_WAITERS] = {
        let mut waiters = n.waiters.lock();
        let woken = *waiters;
        *waiters = [0; MAX_NOTIFY_WAITERS];
        woken
    };
    for w in woken {
        if w != 0 {
            crate::sched::wake_thread(w - 1);
        }
    }
    true
}

/// Non-blocking `swap(0)`. Returns the mask that was pending, or 0.
pub fn notify_poll(nid: u16) -> Option<u64> {
    notification(nid).map(|n| n.bits.swap(0, Ordering::AcqRel))
}

/// Block until at least one event bit is pending, then atomically consume
/// the whole mask and return it.
pub fn notify_wait(nid: u16) -> Option<u64> {
    let n = notification(nid)?;
    let me = crate::sched::current_id();
    loop {
        let mask = n.bits.swap(0, Ordering::AcqRel);
        if mask != 0 {
            return Some(mask);
        }
        {
            let mut waiters = n.waiters.lock();
            let mut registered = false;
            for w in waiters.iter_mut() {
                if *w == me + 1 {
                    registered = true;
                    break;
                }
                if *w == 0 {
                    *w = me + 1;
                    registered = true;
                    break;
                }
            }
            if !registered {
                // Queue is full: spin until a slot frees up rather than
                // losing the wakeup.
                drop(waiters);
                crate::sched::yield_now();
                continue;
            }
        }
        // Re-check after registering: a signal between the swap above and
        // the registration must not be slept through.
        if n.bits.load(Ordering::Acquire) != 0 {
            unregister_waiter(n, me);
            continue;
        }
        crate::sched::block_current();
        unregister_waiter(n, me);
    }
}

fn unregister_waiter(n: &Notification, tid: ThreadId) {
    let mut waiters = n.waiters.lock();
    for w in waiters.iter_mut() {
        if *w == tid + 1 {
            *w = 0;
        }
    }
}

// --- Process exit/fault bindings -------------------------------------------

/// Source of a process binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindSource {
    /// Clean `SYS_EXIT`.
    Exit,
    /// Ring-3 fault.
    Fault,
}

#[derive(Clone, Copy)]
struct ProcBinding {
    nid: u16,
    tid: ThreadId,
    badge: u64,
    source: BindSource,
}

static PROC_BINDINGS: Mutex<[Option<ProcBinding>; MAX_PROC_BINDINGS]> =
    Mutex::new([None; MAX_PROC_BINDINGS]);
static IRQ_NOTIFY_ROUTES: Mutex<[Option<(u16, u64)>; MAX_IRQS]> = Mutex::new([None; MAX_IRQS]);

/// Bind an IRQ line to a notification. One owner per line.
pub fn bind_irq_notification(irq: u8, nid: u16, badge: u64) -> bool {
    let mut routes = IRQ_NOTIFY_ROUTES.lock();
    let Some(slot) = routes.get_mut(irq as usize) else {
        return false;
    };
    if slot.is_some() {
        return false;
    }
    *slot = Some((nid, badge));
    true
}

/// Bind a child process exit/fault to a notification. If the process is
/// already dead, the notification is signalled immediately so a
/// spawn→bind→crash race cannot lose the event.
pub fn bind_process_notification(nid: u16, tid: ThreadId, badge: u64, source: BindSource) -> bool {
    if notification(nid).is_none() {
        return false;
    }
    let mut bindings = PROC_BINDINGS.lock();
    let Some(slot) = bindings.iter_mut().find(|b| b.is_none()) else {
        return false;
    };
    *slot = Some(ProcBinding {
        nid,
        tid,
        badge,
        source,
    });
    drop(bindings);
    if crate::sched::is_dead(tid) {
        let _ = notify_signal(nid, badge);
    }
    true
}

fn signal_process_death(tid: ThreadId, source: BindSource) {
    let mut matches: [(u16, u64); MAX_PROC_BINDINGS] = [(0, 0); MAX_PROC_BINDINGS];
    let mut count = 0usize;
    {
        let mut bindings = PROC_BINDINGS.lock();
        for slot in bindings.iter_mut() {
            let Some(b) = *slot else {
                continue;
            };
            if b.tid != tid {
                continue;
            }
            if b.source == source && count < matches.len() {
                matches[count] = (b.nid, b.badge);
                count += 1;
            }
            // A dead process has no future exit/fault events; drop both
            // the matching and the complementary binding.
            *slot = None;
        }
    }
    for (nid, badge) in matches.iter().take(count) {
        let _ = notify_signal(*nid, *badge);
    }
}

// --- One-shot reply capabilities -------------------------------------------

const REPLY_FREE: u8 = 0;
const REPLY_PENDING: u8 = 1;
const REPLY_DONE: u8 = 2;
const REPLY_FAILED: u8 = 3;

/// One in-flight synchronous request.
pub struct ReplySlot {
    state: AtomicU8,
    client: AtomicU64,
    client_pml4: AtomicU64,
    reply_ptr: AtomicU64,
    reply_max: AtomicU64,
    /// Thread id + 1 of the server that received the call, 0 until then.
    server: AtomicU64,
    result_len: AtomicU64,
}

impl ReplySlot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(REPLY_FREE),
            client: AtomicU64::new(0),
            client_pml4: AtomicU64::new(0),
            reply_ptr: AtomicU64::new(0),
            reply_max: AtomicU64::new(0),
            server: AtomicU64::new(0),
            result_len: AtomicU64::new(0),
        }
    }
}

static REPLIES: [ReplySlot; MAX_REPLIES] = [const { ReplySlot::new() }; MAX_REPLIES];

/// Allocate a reply slot for a blocked client. Returns `None` when the
/// pool is exhausted.
pub fn alloc_reply(client: ThreadId, client_pml4: u64, ptr: u64, max: u64) -> Option<u16> {
    for (i, r) in REPLIES.iter().enumerate() {
        if r.state
            .compare_exchange(
                REPLY_FREE,
                REPLY_PENDING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            r.client.store(client, Ordering::Release);
            r.client_pml4.store(client_pml4, Ordering::Release);
            r.reply_ptr.store(ptr, Ordering::Release);
            r.reply_max.store(max, Ordering::Release);
            r.server.store(0, Ordering::Release);
            r.result_len.store(0, Ordering::Release);
            return Some(i as u16);
        }
    }
    None
}

/// Mark a reply as served by `server` and expose it to `SYS_REPLY`.
pub fn reply_attach_server(id: u16, server: ThreadId) {
    if let Some(r) = REPLIES.get(id as usize) {
        r.server.store(server + 1, Ordering::Release);
    }
}

/// Return `true` when the reply is still pending and belongs to `server`.
pub fn reply_is_owned_by(id: u16, server: ThreadId) -> bool {
    REPLIES
        .get(id as usize)
        .map(|r| {
            r.state.load(Ordering::Acquire) == REPLY_PENDING
                && r.server.load(Ordering::Acquire) == server + 1
        })
        .unwrap_or(false)
}

/// Copy the reply into the blocked client's address space, mark the slot
/// done and wake the client. Returns the number of bytes delivered.
pub fn reply_complete(id: u16, data: &[u8]) -> Result<u64, ()> {
    let Some(r) = REPLIES.get(id as usize) else {
        return Err(());
    };
    if r.state.load(Ordering::Acquire) != REPLY_PENDING {
        return Err(());
    }
    let max = r.reply_max.load(Ordering::Acquire) as usize;
    let n = core::cmp::min(data.len(), max);
    let pml4 = r.client_pml4.load(Ordering::Acquire);
    let ptr = r.reply_ptr.load(Ordering::Acquire);
    crate::syscall::copy_to_user_in(pml4, ptr, &data[..n])?;
    r.result_len.store(n as u64, Ordering::Release);
    r.state.store(REPLY_DONE, Ordering::Release);
    crate::sched::wake_thread(r.client.load(Ordering::Acquire));
    Ok(n as u64)
}

/// Fail a reply and wake the client with an error.
pub fn reply_fail(id: u16) {
    if let Some(r) = REPLIES.get(id as usize) {
        r.state.store(REPLY_FAILED, Ordering::Release);
        let client = r.client.load(Ordering::Acquire);
        if client != 0 {
            crate::sched::wake_thread(client);
        }
    }
}

fn release_reply(id: u16) {
    if let Some(r) = REPLIES.get(id as usize) {
        r.state.store(REPLY_FREE, Ordering::Release);
        r.server.store(0, Ordering::Release);
        r.client.store(0, Ordering::Release);
    }
}

/// Release a reply slot without waking anyone. Used when the request could
/// not even be queued.
pub fn reply_abort(id: u16) {
    release_reply(id);
}

/// Block until the reply is done. Returns the payload length or `None`
/// when the call failed (server died, copy error, no free slot).
pub fn reply_wait(id: u16) -> Option<u64> {
    let r = REPLIES.get(id as usize)?;
    loop {
        match r.state.load(Ordering::Acquire) {
            REPLY_DONE => {
                let n = r.result_len.load(Ordering::Acquire);
                release_reply(id);
                return Some(n);
            }
            REPLY_FAILED => {
                release_reply(id);
                return None;
            }
            _ => crate::sched::block_current(),
        }
    }
}

// --- Process death ----------------------------------------------------------

/// Why a process stopped running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeathReason {
    /// Clean `SYS_EXIT`.
    Exit,
    /// Ring-3 fault.
    Fault,
}

/// Single cleanup point for a dying process: notify supervisors, fail
/// pending calls, revoke every capability it owns (and their descendants)
/// and release DMA/shared mappings.
pub fn process_died(tid: ThreadId, reason: DeathReason) {
    crate::sched::mark_died(tid);
    let source = match reason {
        DeathReason::Exit => BindSource::Exit,
        DeathReason::Fault => BindSource::Fault,
    };
    signal_process_death(tid, source);
    for (id, r) in REPLIES.iter().enumerate() {
        if r.state.load(Ordering::Acquire) == REPLY_PENDING {
            let server = r.server.load(Ordering::Acquire);
            if server == tid + 1 {
                reply_fail(id as u16);
            }
            let client = r.client.load(Ordering::Acquire);
            if client == tid {
                release_reply(id as u16);
            }
        }
    }
    crate::cdt::revoke_owned(tid);
    crate::dma::free_for(tid);
}

// --- IRQ routing ------------------------------------------------------------

/// Endpoint that receives one-byte "IRQ n fired" messages, per IRQ line.
/// Index is the legacy ISA IRQ number (0..=15).
static IRQ_ROUTES: Mutex<[Option<u64>; MAX_IRQS]> = Mutex::new([None; MAX_IRQS]);

/// Bind an IRQ line to an endpoint. Returns false for out-of-range or
/// already-bound lines (one owner per line, capabilities are not shared
/// implicitly).
pub fn route_irq(irq: u8, ep_id: u64) -> bool {
    let mut routes = IRQ_ROUTES.lock();
    let Some(slot) = routes.get_mut(irq as usize) else {
        return false;
    };
    if slot.is_some() {
        return false;
    }
    *slot = Some(ep_id);
    true
}

/// Called from an IRQ handler with interrupts disabled: enqueue a
/// notification on the registered endpoint. The IPC and scheduler locks
/// are safe here because every other holder runs with IF=0 (syscalls via
/// SFMASK, scheduler paths via `without_interrupts`).
pub fn deliver_irq(irq: u8) {
    if let Some(Some((nid, badge))) = IRQ_NOTIFY_ROUTES.lock().get(irq as usize) {
        let _ = notify_signal(*nid, *badge);
    }
    let ep_id = match IRQ_ROUTES.lock().get(irq as usize) {
        Some(Some(id)) => *id,
        _ => return,
    };
    let mut data = [0u8; MAX_MSG_LEN];
    data[0] = irq;
    // A full queue means the driver is not draining its endpoint; the IRQ
    // line stays masked until it does, so dropping the note is safe.
    let _ = send(ep_id, Message::plain(data, 1, 0));
}
