//! NovaX v0.2 supervisor: capability lifecycle + asynchronous notifications.
//!
//! Wiring (capabilities granted by the kernel at boot):
//!   cap 1 = Endpoint(v02_ep)        R/W
//!   cap 2 = Notification            R/W/NOTIFY
//!   cap 3 = Dma(4096)               R/W
//!   cap 4 = Endpoint(v02_go_ep)     R/W
//!
//! The supervisor owns the resource endpoint and a shared buffer, spawns a
//! server and a client, observes the server's fault through a notification,
//! revokes the buffer capability (unmapping it from the live client) and
//! spawns a replacement server.

#![no_std]
#![no_main]

#[path = "../../common/abi.rs"]
mod abi;

const CAP_EP: u64 = 1;
const CAP_NOTIFY: u64 = 2;
const CAP_DMA: u64 = 3;
const CAP_GO: u64 = 4;
const CAP_DONE: u64 = 5;

const BADGE_EXIT: u64 = 0x1;
const BADGE_FAULT: u64 = 0x2;

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

fn log(s: &[u8]) {
    abi::write(s);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    log(b"v02: supervisor ready\n");

    // 1. Allocate the buffer the client will map read-only.
    let mut dma = [0u8; 16];
    if abi::dma_alloc(CAP_DMA, &mut dma) != 0 {
        log(b"v02: dma_alloc failed\n");
        abi::exit(1);
    }
    let dma_virt = u64::from_le_bytes(dma[0..8].try_into().unwrap());
    // SAFETY: the kernel mapped a writable 4 KiB DMA window at `dma_virt`.
    unsafe {
        core::ptr::copy_nonoverlapping(b"shared".as_ptr(), dma_virt as *mut u8, 6);
    }

    // 2. Mint a read-only shared view and hand it to the client.
    let mut shm_slot = u64::MAX;
    if abi::cap_mint(CAP_DMA, abi::CAP_READ, &mut shm_slot) != 0 {
        log(b"v02: cap_mint failed\n");
        abi::exit(1);
    }

    // 3. Spawn the server with a reduced endpoint capability.
    let server_grants = [abi::CapGrant {
        src_slot: CAP_EP as u32,
        rights: abi::CAP_READ,
    }];
    let mut server_meta = abi::SpawnMeta {
        proc_slot: -1,
        reserved: 0,
    };
    if abi::spawn(abi::PROG_V02_SERVER, &server_grants, &mut server_meta) != 0 {
        log(b"v02: server spawn failed\n");
        abi::exit(1);
    }
    if abi::notify_bind(
        CAP_NOTIFY,
        abi::NOTIFY_SRC_EXIT,
        server_meta.proc_slot as u64,
        BADGE_EXIT,
    ) != 0
    {
        log(b"v02: bind exit failed\n");
    }
    if abi::notify_bind(
        CAP_NOTIFY,
        abi::NOTIFY_SRC_FAULT,
        server_meta.proc_slot as u64,
        BADGE_FAULT,
    ) != 0
    {
        log(b"v02: bind fault failed\n");
    }
    log(b"v02: server spawned\n");

    // 4. Spawn the client: endpoint write, its own DMA, the shared view and
    //    the control endpoint.
    let client_grants = [
        abi::CapGrant {
            src_slot: CAP_EP as u32,
            rights: abi::CAP_WRITE,
        },
        abi::CapGrant {
            src_slot: CAP_DMA as u32,
            rights: abi::CAP_READ | abi::CAP_WRITE,
        },
        abi::CapGrant {
            src_slot: shm_slot as u32,
            rights: abi::CAP_READ,
        },
        abi::CapGrant {
            src_slot: CAP_GO as u32,
            rights: abi::CAP_READ,
        },
        abi::CapGrant {
            src_slot: CAP_DONE as u32,
            rights: abi::CAP_WRITE,
        },
    ];
    let mut client_meta = abi::SpawnMeta {
        proc_slot: -1,
        reserved: 0,
    };
    if abi::spawn(abi::PROG_V02_CLIENT, &client_grants, &mut client_meta) != 0 {
        log(b"v02: client spawn failed\n");
        abi::exit(1);
    }
    log(b"v02: client spawned\n");

    // 5. Wait for the server fault.
    let badge = abi::notify_wait(CAP_NOTIFY);
    if badge & BADGE_FAULT != 0 {
        log(b"v02: server fault badge=0x2\n");
    } else {
        log(b"v02: unexpected notification\n");
    }

    // 6. Revoke every capability derived from the shared buffer. This also
    //    unmaps the live client's read-only window.
    abi::cap_revoke(shm_slot);
    log(b"v02: revoked shared buffer\n");

    // 7. Replacement server.
    let mut replacement = abi::SpawnMeta {
        proc_slot: -1,
        reserved: 0,
    };
    if abi::spawn(abi::PROG_V02_SERVER, &server_grants, &mut replacement) != 0 {
        log(b"v02: replacement spawn failed\n");
        abi::exit(1);
    }
    if abi::notify_bind(
        CAP_NOTIFY,
        abi::NOTIFY_SRC_FAULT,
        replacement.proc_slot as u64,
        BADGE_FAULT,
    ) != 0
    {
        log(b"v02: rebind fault failed\n");
    }
    log(b"v02: server respawned\n");

    // 8. Let the client continue and wait for its completion message.
    abi::send(CAP_GO, b"go");
    let mut done = [0u8; 16];
    let n = abi::recv(CAP_DONE, &mut done);
    if n == 4 && &done[..4] == b"done" {
        log(b"v02: PASS\n");
    } else {
        log(b"v02: FAIL\n");
    }
    abi::exit(0)
}
