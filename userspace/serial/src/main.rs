//! NovaX serial server: owns COM1 (IRQ4) and echoes received bytes.
//!
//! All hardware access goes through capability-checked syscalls:
//!   cap 1 = Irq(4)                      (CAP_NOTIFY)
//!   cap 2 = Endpoint(irq endpoint)      (CAP_READ | CAP_WRITE)
//!   cap 3 = IoPort { base: 0x3F8, len: 8 } (CAP_READ | CAP_WRITE)
//!   cap 4 = Endpoint(shell input)       (CAP_WRITE)

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

const CAP_IRQ: u64 = 1;
const CAP_EP: u64 = 2;
const CAP_IO: u64 = 3;
const CAP_SHELL: u64 = 4;

const REG_RBR: u64 = 0; // receive buffer / transmit holding
const REG_IER: u64 = 1;
const REG_LSR: u64 = 5;

const LSR_DATA_READY: u64 = 0x01;
const LSR_THR_EMPTY: u64 = 0x20;

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
    abi::write(b"serial: driver starting (IRQ4)\n");

    if abi::irq_register(CAP_IRQ, CAP_EP) != 0 {
        abi::write(b"serial: irq_register failed\n");
        abi::exit(1);
    }

    // Enable "received data available" interrupt in the UART.
    if abi::outb(CAP_IO, REG_IER, 0x01) == u64::MAX {
        abi::write(b"serial: IER write denied\n");
        abi::exit(1);
    }

    // EOI + unmask: arm IRQ4 now that the device is ready.
    abi::irq_ack(CAP_IRQ);
    abi::write(b"serial: ready\n");

    let mut msg = [0u8; 8];
    loop {
        let n = abi::recv(CAP_EP, &mut msg);
        if n == u64::MAX {
            continue;
        }

        // Drain every byte the UART has buffered before acking.
        loop {
            let lsr = abi::inb(CAP_IO, REG_LSR);
            if lsr == u64::MAX || lsr & LSR_DATA_READY == 0 {
                break;
            }

            let b = abi::inb(CAP_IO, REG_RBR);
            if b == u64::MAX {
                break;
            }

            // Wait for THR to drain (kernel may be printing concurrently).
            let mut spins = 0u32;
            while spins < 100_000 {
                let s = abi::inb(CAP_IO, REG_LSR);
                if s & LSR_THR_EMPTY != 0 {
                    break;
                }
                spins += 1;
            }
            abi::outb(CAP_IO, REG_RBR, b); // echo
            write_hex(b"serial: rx 0x", b as u8);
            // Forward the raw byte to the shell console endpoint.
            let bv = b as u8;
            abi::send(CAP_SHELL, &[bv]);
        }

        abi::irq_ack(CAP_IRQ);
    }
}
