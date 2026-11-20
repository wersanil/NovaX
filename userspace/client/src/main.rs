//! NovaX M5 demo: IPC sender.
//!
//! Sends `ping` through capability 1 and exits. The matching receiver is
//! `userspace/server`.

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
    abi::write(b"client: sending ping\n");
    abi::send(1, b"ping");
    abi::exit(0);
}
