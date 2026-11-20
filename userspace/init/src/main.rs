//! NovaX init server: the first system service.
//!
//! It owns the command endpoint and answers requests from the shell:
//!   "echo <text>" -> "<text>"
//!   anything else -> "unknown command"
//!
//! Wiring (capabilities granted by the kernel at spawn):
//!   cap 1 = Endpoint(command endpoint)  (CAP_READ)
//!   cap 2 = Endpoint(shell reply)       (CAP_WRITE)
//!
//! init does not print anything itself: the reply must travel over IPC
//! and be rendered by the shell, which is what M7 demonstrates.

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

const CAP_CMD: u64 = 1;
const CAP_REPLY: u64 = 2;

const ECHO_PREFIX: &[u8] = b"echo ";
const UNKNOWN: &[u8] = b"unknown command";

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut msg = [0u8; 64];
    loop {
        let n = abi::recv(CAP_CMD, &mut msg);
        if n == u64::MAX || n == 0 {
            continue;
        }

        let cmd = &msg[..n as usize];
        let reply: &[u8] =
            if cmd.len() >= ECHO_PREFIX.len() && &cmd[..ECHO_PREFIX.len()] == ECHO_PREFIX {
                &cmd[ECHO_PREFIX.len()..]
            } else {
                UNKNOWN
            };
        abi::send(CAP_REPLY, reply);
    }
}
