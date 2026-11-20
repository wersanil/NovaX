//! NovaX PS/2 keyboard server: owns IRQ1 and reports scancodes.
//!
//! All hardware access goes through capability-checked syscalls:
//!   cap 1 = Irq(1)                         (CAP_NOTIFY)
//!   cap 2 = Endpoint(irq endpoint)         (CAP_READ | CAP_WRITE)
//!   cap 3 = IoPort { base: 0x60, len: 5 }  (CAP_READ | CAP_WRITE)

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

const CAP_IRQ: u64 = 1;
const CAP_EP: u64 = 2;
const CAP_IO: u64 = 3;

const REG_DATA: u64 = 0; // 0x60: scancode
const REG_STATUS: u64 = 4; // 0x64: bit 0 = output buffer full

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

fn write_hex(prefix: &[u8], v: u8) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let line = [DIGITS[(v >> 4) as usize], DIGITS[(v & 0xF) as usize], b'\n'];
    abi::write(prefix);
    abi::write(&line);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    abi::write(b"kbd: driver starting (IRQ1)\n");

    if abi::irq_register(CAP_IRQ, CAP_EP) != 0 {
        abi::write(b"kbd: irq_register failed\n");
        abi::exit(1);
    }

    // EOI + unmask: arm IRQ1.
    abi::irq_ack(CAP_IRQ);
    abi::write(b"kbd: ready\n");

    let mut msg = [0u8; 8];
    loop {
        let n = abi::recv(CAP_EP, &mut msg);
        if n == u64::MAX {
            continue;
        }

        // Drain the controller's output buffer (it may hold several bytes).
        loop {
            let st = abi::inb(CAP_IO, REG_STATUS);
            if st == u64::MAX || st & 0x01 == 0 {
                break;
            }

            let sc = abi::inb(CAP_IO, REG_DATA);
            if sc == u64::MAX {
                break;
            }
            write_hex(b"kbd: scancode 0x", sc as u8);
        }

        abi::irq_ack(CAP_IRQ);
    }
}
