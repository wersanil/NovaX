//! Kernel IPC: endpoints, fixed-size messages, capabilities and IRQ routes.
//!
//! An endpoint has a bounded FIFO (messages ≤ [`MAX_MSG_LEN`]) and at most
//! one blocked receiver. IRQ delivery is expressed as a one-byte message
//! on an endpoint bound with [`route_irq`], so drivers never need a
//! kernel-side callback.

use crate::sched::ThreadId;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use spin::Mutex;

/// A capability held in a thread's cspace (16 slots per thread).
#[derive(Clone, Copy, Debug)]
pub struct Capability {
    /// What the capability grants access to.
    pub kind: CapKind,
    /// Bit mask of `CAP_*` rights required by the operation.
    pub rights: u32,
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
}

/// Right to receive from an endpoint.
pub const CAP_READ: u32 = 1;
/// Right to send to an endpoint or write an I/O port.
pub const CAP_WRITE: u32 = 2;
/// Right to register an IRQ capability for kernel-to-userspace delivery.
pub const CAP_NOTIFY: u32 = 4;

/// Maximum payload of one IPC message.
pub const MAX_MSG_LEN: usize = 64;
/// Number of legacy ISA IRQ lines the router tracks.
pub const MAX_IRQS: usize = 16;

/// One queued IPC message. `len` bytes of `data` are meaningful.
#[derive(Clone, Copy)]
pub struct Message {
    /// Payload bytes.
    pub data: [u8; MAX_MSG_LEN],
    /// Number of valid bytes in `data`.
    pub len: usize,
    /// Thread that sent the message (0 for kernel-originated IRQ notes).
    #[allow(dead_code)]
    pub sender: ThreadId,
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
pub fn send(ep_id: u64, msg: Message) {
    let mut ipc = IPC.lock();
    if let Some(ep) = ipc.endpoints.iter_mut().find(|e| e.id == ep_id) {
        ep.queue.push_back(msg);
        if let Some(tid) = ep.blocked_receiver.take() {
            drop(ipc);
            crate::sched::wake_thread(tid);
        }
    }
}

/// Pop one message from `ep_id`, or register the caller as the blocked
/// receiver. Returns `None` in the latter case (the caller then blocks).
pub fn recv(ep_id: u64) -> Option<Message> {
    let mut ipc = IPC.lock();
    if let Some(ep) = ipc.endpoints.iter_mut().find(|e| e.id == ep_id) {
        if let Some(msg) = ep.queue.pop_front() {
            return Some(msg);
        }
        ep.blocked_receiver = Some(crate::sched::current_id());
    }
    None
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
    let ep_id = match IRQ_ROUTES.lock().get(irq as usize) {
        Some(Some(id)) => *id,
        _ => return,
    };
    let mut data = [0u8; MAX_MSG_LEN];
    data[0] = irq;
    send(
        ep_id,
        Message {
            data,
            len: 1,
            sender: 0,
        },
    );
}
