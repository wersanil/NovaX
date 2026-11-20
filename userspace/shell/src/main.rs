//! NovaX shell server.
//!
//! Input path:   serial driver -> shell input endpoint -> line buffer
//! Command path: shell -> init command endpoint (SYS_SEND)
//! Reply path:   init -> shell reply endpoint (SYS_SEND) -> shell prints
//!
//! Wiring (capabilities granted by the kernel at spawn):
//!   cap 1 = Endpoint(init command)   (CAP_WRITE)
//!   cap 2 = Endpoint(shell input)    (CAP_READ)
//!   cap 3 = Endpoint(shell reply)    (CAP_READ)

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

const CAP_INIT: u64 = 1;
const CAP_IN: u64 = 2;
const CAP_REPLY: u64 = 3;

const LINE_MAX: usize = 64;
const OUT_MAX: usize = 128;

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Render `prefix + body + "\n"` in one syscall so concurrent kernel log
/// lines cannot split the message.
fn write_line(prefix: &[u8], body: &[u8]) {
    let mut out = [0u8; OUT_MAX];
    let mut n = 0;
    for &b in prefix.iter().chain(body.iter()).chain(b"\n".iter()) {
        if n == out.len() {
            break;
        }
        out[n] = b;
        n += 1;
    }
    abi::write(&out[..n]);
}

fn run_command(line: &[u8]) {
    write_line(b"shell: > ", line);

    if abi::send(CAP_INIT, line) == u64::MAX {
        abi::write(b"shell: send to init failed\n");
        return;
    }

    let mut reply = [0u8; LINE_MAX];
    let n = abi::recv(CAP_REPLY, &mut reply);
    if n == u64::MAX || n == 0 {
        abi::write(b"shell: no reply from init\n");
        return;
    }
    write_line(b"shell: result=", &reply[..n as usize]);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    abi::write(b"shell: ready\n");

    let mut line = [0u8; LINE_MAX];
    let mut len = 0usize;
    let mut byte = [0u8; 1];

    loop {
        let n = abi::recv(CAP_IN, &mut byte);
        if n == u64::MAX || n == 0 {
            continue;
        }

        match byte[0] {
            b'\r' | b'\n' => {
                if len > 0 {
                    run_command(&line[..len]);
                    len = 0;
                }
            }
            b if len < LINE_MAX => {
                line[len] = b;
                len += 1;
            }
            _ => {
                // Line overflow: drop the partial command.
                len = 0;
            }
        }
    }
}
