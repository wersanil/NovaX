//! Kernel console mirror.
//!
//! Every byte written to COM1 is also offered to the ring-3 `fb` server.
//! Delivery is strictly best-effort: [`crate::ipc::try_send`] never waits for
//! the fb queue, and overflow is dropped. The serial console must keep
//! working even if the graphical server is stalled.

use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::ipc::{self, Message, MAX_MSG_LEN};

/// Bytes of boot output retained until the fb server attaches.
const BOOT_LOG_CAP: usize = 32 * 1024;
/// Sentinel meaning "no graphical sink attached yet".
const NO_SINK: u64 = u64::MAX;

struct BootLog {
    buf: [u8; BOOT_LOG_CAP],
    len: usize,
}

impl BootLog {
    const fn new() -> Self {
        Self {
            buf: [0; BOOT_LOG_CAP],
            len: 0,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        let room = BOOT_LOG_CAP - self.len;
        let n = core::cmp::min(bytes.len(), room);
        self.buf[self.len..self.len + n].copy_from_slice(&bytes[..n]);
        self.len += n;
        // When the buffer is full new bytes are dropped; the screen is a
        // mirror, not the primary console.
    }
}

static BOOT_LOG: Mutex<BootLog> = Mutex::new(BootLog::new());
static SINK_EP: AtomicU64 = AtomicU64::new(NO_SINK);

fn message(chunk: &[u8]) -> Message {
    let mut data = [0u8; MAX_MSG_LEN];
    data[..chunk.len()].copy_from_slice(chunk);
    Message {
        data,
        len: chunk.len(),
        sender: 0,
    }
}

fn offer(ep: u64, bytes: &[u8]) {
    for chunk in bytes.chunks(MAX_MSG_LEN) {
        // WHY: console mirroring is opportunistic; a full queue or a busy
        // IPC lock must never stall the serial driver or the kernel.
        let _ = ipc::try_send(ep, message(chunk));
    }
}

/// Mirror `bytes` to the fb server, or retain them for the boot replay.
///
/// Called from `serial::_print` with interrupts disabled. Uses `try_lock`
/// so a panic that interrupts another console operation cannot deadlock.
pub fn push(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    let ep = SINK_EP.load(Ordering::Acquire);
    if ep != NO_SINK {
        offer(ep, bytes);
        return;
    }

    if let Some(mut log) = BOOT_LOG.try_lock() {
        log.append(bytes);
    }
}

/// Attach the fb console endpoint and replay everything printed so far.
///
/// Called once by the boot thread before interrupts are enabled, so the
/// boot log is quiescent while it is drained.
#[cfg(not(feature = "test"))]
pub fn attach(ep: u64) {
    let mut log = BOOT_LOG.lock();
    offer(ep, &log.buf[..log.len]);
    log.len = 0;
    SINK_EP.store(ep, Ordering::Release);
}
