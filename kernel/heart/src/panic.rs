//! Fail-fast panic handler. NovaX has no silent degradation.

use core::panic::PanicInfo;

use crate::{halt_forever, serial_print};

#[panic_handler]
fn on_panic(info: &PanicInfo<'_>) -> ! {
    // Interrupts off — we're about to freeze the CPU and don't want
    // a timer tick to preempt us mid-print.
    // SAFETY: `cli` is privileged; we are in ring 0 in the panic path.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
    }

    serial_print!("\n*** NovaX KERNEL PANIC ***\n");
    if let Some(loc) = info.location() {
        serial_print!("at {}:{}:{}\n", loc.file(), loc.line(), loc.column());
    }
    serial_print!("{}\n", info.message());
    serial_print!("*** halted ***\n");

    halt_forever()
}
