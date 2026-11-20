//! NovaX M5 demo: IPC receiver.
//!
//! Waits for one message on capability 1 (the shared endpoint), prints it
//! and exits.

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
    abi::write(b"server: waiting for message\n");
    let n = abi::recv(1, &mut buf);
    if n > 0 && n <= 64 {
        abi::write(b"server: got message: ");
        abi::write(&buf[..n as usize]);
        abi::write(b"\n");
    }
    abi::exit(0);
}
