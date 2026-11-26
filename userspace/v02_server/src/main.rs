//! NovaX v0.2 test server.
//!
//! cap 0 = Endpoint(v02_ep) READ.
//!
//! Receives one synchronous call carrying a read-only buffer capability,
//! maps it, verifies the payload and replies through the one-shot reply
//! capability. It then deliberately faults so the supervisor can observe
//! the fault notification and exercise revoke + respawn.

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut buf = [0u8; 64];
    let mut meta = abi::RecvMeta::empty();
    let n = abi::recv_cap(0, &mut buf, &mut meta);
    if n == u64::MAX || meta.reply_slot < 0 {
        abi::write(b"v02_server: bad request\n");
        abi::exit(1);
    }

    let mut ok = false;
    if meta.cap_slot >= 0 {
        let mut info = abi::MapInfo {
            virt: 0,
            phys: 0,
            len: 0,
        };
        if abi::map_shared(meta.cap_slot as u64, &mut info) == 0 {
            // SAFETY: the kernel mapped a read-only shared page at `virt`.
            let first = unsafe { core::ptr::read_volatile(info.virt as *const u8) };
            ok = first == b'p';
        }
    }

    let reply = if ok {
        b"pong".as_slice()
    } else {
        b"bad ".as_slice()
    };
    abi::reply(meta.reply_slot as u64, reply);

    // Crash after the reply: a ring-3 page fault must notify the supervisor
    // and revoke this process's capabilities/mappings.
    // SAFETY: intentional invalid access; the kernel terminates this thread.
    unsafe {
        core::ptr::write_volatile(0xdead_0000 as *mut u64, 1);
    }
    abi::exit(42)
}
