//! NovaX v0.2 test client.
//!
//! cap 0 = Endpoint(v02_ep)          WRITE
//! cap 1 = Dma(4096)                 R/W
//! cap 2 = Shm(supervisor buffer)    READ
//! cap 3 = Endpoint(v02_go_ep)       R/W
//!
//! Performs a synchronous call carrying a read-only capability on its own
//! buffer, then waits for the supervisor's `go`, verifies that the
//! supervisor's shared window was revoked and unmapped, and performs a
//! second call against the replacement server.

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

const CAP_EP: u64 = 0;
const CAP_DMA: u64 = 1;
const CAP_SUP_SHM: u64 = 2;
const CAP_GO: u64 = 3;
const CAP_DONE: u64 = 4;

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

fn log(s: &[u8]) {
    abi::write(s);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // 1. Own buffer + read-only capability to transfer.
    let mut dma = [0u8; 16];
    if abi::dma_alloc(CAP_DMA, &mut dma) != 0 {
        log(b"v02_client: dma_alloc failed\n");
        abi::exit(1);
    }
    let dma_virt = u64::from_le_bytes(dma[0..8].try_into().unwrap());
    // SAFETY: the kernel mapped a writable 4 KiB DMA window at `dma_virt`.
    unsafe {
        core::ptr::copy_nonoverlapping(b"ping".as_ptr(), dma_virt as *mut u8, 4);
    }
    let mut shm_slot = u64::MAX;
    if abi::cap_mint(CAP_DMA, abi::CAP_READ, &mut shm_slot) != 0 {
        log(b"v02_client: cap_mint failed\n");
        abi::exit(1);
    }

    // 2. Map the supervisor's buffer read-only; it must disappear after
    //    SYS_CAP_REVOKE.
    let mut shared = abi::MapInfo {
        virt: 0,
        phys: 0,
        len: 0,
    };
    if abi::map_shared(CAP_SUP_SHM, &mut shared) != 0 {
        log(b"v02_client: map_shared failed\n");
        abi::exit(1);
    }

    // 3. First synchronous call with the buffer capability attached.
    let mut reply = [0u8; 64];
    let n = abi::call(
        CAP_EP,
        shm_slot as u16,
        abi::CAP_READ as u16,
        b"ping",
        &mut reply,
    );
    if n == 4 && &reply[..4] == b"pong" {
        log(b"v02: client request 1 -> pong\n");
    } else {
        log(b"v02: client request 1 failed\n");
    }

    // 4. Wait for the supervisor to revoke and respawn.
    let mut go = [0u8; 16];
    let _ = abi::recv(CAP_GO, &mut go);

    // 5. The supervisor's shared window must now be unmapped. Writing to
    //    it through SYS_WRITE returns u64::MAX instead of faulting.
    let probe = unsafe { abi::syscall3(abi::SYS_WRITE, 1, shared.virt, 1) };
    if probe == u64::MAX {
        log(b"v02: client shm gone\n");
    } else {
        log(b"v02: client shm still mapped\n");
    }

    // 6. Second call against the replacement server.
    let mut reply2 = [0u8; 64];
    let n = abi::call(
        CAP_EP,
        shm_slot as u16,
        abi::CAP_READ as u16,
        b"ping",
        &mut reply2,
    );
    if n == 4 && &reply2[..4] == b"pong" {
        log(b"v02: client request 2 -> pong\n");
    } else {
        log(b"v02: client request 2 failed\n");
    }

    abi::send(CAP_DONE, b"done");
    abi::exit(0)
}
