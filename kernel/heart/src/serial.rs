//! 16550 UART driver for COM1. The only I/O channel in Milestone 0.

use core::fmt;
use spin::Mutex;

const COM1: u16 = 0x3F8;

/// # Safety
///
/// Raw port I/O; only legal in ring 0 with a valid UART port.
#[inline(always)]
unsafe fn outb(port: u16, val: u8) {
    // WHY unsafe: raw port I/O; only legal in ring 0 with correct port.
    unsafe {
        core::arch::asm!("out dx, al", in("dx") port, in("al") val, options(nomem, nostack, preserves_flags));
    }
}

/// # Safety
///
/// Raw port I/O; the caller must know the port is readable.
#[inline(always)]
unsafe fn inb(port: u16) -> u8 {
    // WHY unsafe: raw port I/O.
    let v: u8;
    unsafe {
        core::arch::asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack, preserves_flags));
    }
    v
}

struct Uart {
    base: u16,
}

impl Uart {
    const fn new(base: u16) -> Self {
        Self { base }
    }

    /// Program the UART: 38400 8N1, FIFO on, no IRQs (M2 enables them
    /// from the userspace serial server).
    ///
    /// # Safety
    ///
    /// Privileged port I/O: must run in ring 0 on the fixed COM1 window.
    unsafe fn init(&self) {
        // SAFETY: ring-0 port I/O to the fixed COM1 window; the UART is
        // quiescent while the kernel owns it during boot.
        unsafe {
            outb(self.base + 1, 0x00); // disable IRQs
            outb(self.base + 3, 0x80); // DLAB on
            outb(self.base, 0x03); // divisor lo
            outb(self.base + 1, 0x00); // divisor hi
            outb(self.base + 3, 0x03); // 8N1
            outb(self.base + 2, 0xC7); // FIFO on, clear, 14-byte threshold
            outb(self.base + 4, 0x0B); // IRQs off, RTS/DSR on
        }
    }

    fn write_byte(&self, b: u8) {
        // Line Status Register bit 5 = Transmit Holding Register Empty.
        // SAFETY: ring-0 port I/O to the fixed COM1 window.
        unsafe {
            while inb(self.base + 5) & 0x20 == 0 {
                core::hint::spin_loop();
            }
            outb(self.base, b);
        }
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
        Ok(())
    }
}

static SERIAL: Mutex<Option<Uart>> = Mutex::new(None);

/// Writer that feeds both COM1 and the kernel console mirror.
struct Tee<'a> {
    uart: &'a mut Uart,
}

impl fmt::Write for Tee<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.uart.write_str(s)?;
        crate::console::push(s.as_bytes());
        Ok(())
    }
}

/// Bring up COM1 and publish it to the global writer. Safe to call once.
pub fn init() {
    let uart = Uart::new(COM1);
    // SAFETY: ring 0 during boot; COM1 is reserved for the kernel console
    // until the userspace serial server is spawned.
    unsafe { uart.init() };
    *SERIAL.lock() = Some(uart);
}

/// Internal sink used by the `serial_print*!` macros. Not a stable API.
#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    // Preemption off while holding the UART lock. Otherwise a timer tick
    // could switch to a thread that also wants to print, and it would
    // spin forever on the lock we're holding.
    x86_64::instructions::interrupts::without_interrupts(|| {
        use fmt::Write;
        if let Some(uart) = SERIAL.lock().as_mut() {
            // fmt::Write for the UART sink is infallible; the Result only
            // exists to satisfy the trait. The same formatted text is
            // offered to the graphical console on a best-effort basis.
            let mut tee = Tee { uart };
            let _ = tee.write_fmt(args);
        }
    });
}

#[macro_export]
macro_rules! serial_print {
    ($($t:tt)*) => { $crate::serial::_print(format_args!($($t)*)) };
}

#[macro_export]
macro_rules! serial_println {
    ()            => { $crate::serial_print!("\n") };
    ($($t:tt)*)   => { $crate::serial_print!("{}\n", format_args!($($t)*)) };
}
