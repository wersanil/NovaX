//! First NovaX userspace program: print one line from ring 3 and exit.
//!
//! Kept as an M4 smoke test; the kernel does not spawn it by default.

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

#[panic_handler]
fn on_panic(_info: &core::panic::PanicInfo) -> ! {
    // We cannot print safely from a panic (the panic may be in `write`),
    // so just terminate the thread with a non-zero status.
    abi::exit(1)
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    abi::write(b"Hello from ring 3!\n");
    abi::exit(0);
}
