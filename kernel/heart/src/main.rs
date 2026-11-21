//! NovaX Heart — microkernel.
//!
//! Milestone 1: physical frames, paging, kernel heap, memory tests.

#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod arch;
mod bootinfo;
mod console;
mod dma;
mod heap;
mod ipc;
mod mmio;
mod panic;
#[cfg(not(feature = "test"))]
mod pci;
mod sched;
mod serial;
mod syscall;
mod user;

#[cfg(feature = "test")]
mod tests;

use core::arch::asm;
use spin::Once;

use limine::{RequestsEndMarker, RequestsStartMarker};
use novax_memory::{FrameAllocator, MemoryResult, PageTableRef};
use spin::Mutex;

// ---- Limine request markers -----------------------------------------------

#[used]
#[link_section = ".requests_start_marker"]
static _START_MARKER: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[link_section = ".requests_end_marker"]
static _END_MARKER: RequestsEndMarker = RequestsEndMarker::new();

// ---- Global memory manager ------------------------------------------------

/// Global physical memory manager: frame allocator plus the active paging
/// context used during boot.
pub struct MemoryManager {
    /// Physical frame allocator.
    pub frames: FrameAllocator,
    /// Kernel page tables (CR3 during boot).
    pub paging: PageTableRef,
}

/// Set once by `_start`; all kernel subsystems reach memory through it.
pub static MEMORY: Once<Mutex<MemoryManager>> = Once::new();

// ---- Entry ----------------------------------------------------------------

#[no_mangle]
pub extern "C" fn _start() -> ! {
    serial::init();
    crate::serial_println!("NovaX booting...");

    bootinfo::check_base_revision();
    let info = bootinfo::collect();

    crate::serial_println!("hhdm offset: {:#x}", info.hhdm.as_u64());
    crate::serial_println!("memmap entries: {}", info.region_count);
    if let Some(fb) = info.framebuffer {
        crate::serial_println!(
            "fb: {}x{} pitch={} bpp={} paddr={:#x} len={:#x}",
            fb.width,
            fb.height,
            fb.pitch,
            fb.bpp,
            fb.paddr,
            fb.len
        );
    } else {
        crate::serial_println!("fb: no framebuffer (headless mode)");
    }

    // WHY unsafe: see `FrameAllocator::new` and `PageTableRef::active`
    // safety contracts; both are satisfied by limine-supplied values.
    let frames =
        unsafe { FrameAllocator::new(info.hhdm, info.regions()) }.expect("frame allocator init");
    let paging = unsafe { PageTableRef::active(info.hhdm) }.expect("paging init");

    crate::serial_println!(
        "frames total: {}, free: {}",
        frames.total_frames(),
        frames.free_count()
    );

    MEMORY.call_once(|| Mutex::new(MemoryManager { frames, paging }));

    heap::init().expect("heap init");
    arch::x86_64::init();
    sched::init();
    // Capture kernel PML4 before any user thread runs.
    arch::x86_64::capture_kernel_pml4();

    #[cfg(feature = "test")]
    {
        tests::run_all();
        // SAFETY: ring 0; the QEMU test run is over and must not be
        // interrupted before the isa-debug-exit write.
        unsafe {
            asm!("cli", options(nomem, nostack, preserves_flags));
        }
        exit_qemu(QemuExitCode::Success);
    }

    #[cfg(not(feature = "test"))]
    {
        crate::serial_println!("Heart: alive.");
        crate::serial_println!("Milestone 5: OK");

        // 1. Создаем эндпоинт в ядре
        let ep_id = ipc::create_endpoint();
        crate::serial_println!("kernel: endpoint {}", ep_id);

        // 2. Настраиваем Capability для сервера (доступ к эндпоинту по индексу 1)
        let mut server_caps = [None; 16];
        server_caps[1] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(ep_id),
            rights: ipc::CAP_READ | ipc::CAP_WRITE,
        });

        let server_elf = include_bytes!(concat!(env!("OUT_DIR"), "/server.elf"));
        match user::launch(server_elf, server_caps) {
            Ok(tid) => crate::serial_println!("server thread id = {}", tid),
            Err(e) => panic!("server launch failed: {:?}", e),
        }

        // 3. Настраиваем Capability для клиента
        let mut client_caps = [None; 16];
        client_caps[1] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(ep_id),
            rights: ipc::CAP_READ | ipc::CAP_WRITE,
        });

        let client_elf = include_bytes!(concat!(env!("OUT_DIR"), "/client.elf"));
        match user::launch(client_elf, client_caps) {
            Ok(tid) => crate::serial_println!("client thread id = {}", tid),
            Err(e) => panic!("client launch failed: {:?}", e),
        }

        // 4. M7 wiring: init owns commands, shell talks to it and receives
        //    console bytes from the serial driver.
        let init_ep = ipc::create_endpoint();
        let shell_in_ep = ipc::create_endpoint();
        let shell_reply_ep = ipc::create_endpoint();

        // 5. Serial driver as a userspace server: IRQ4 + COM1 port window.
        let serial_ep = ipc::create_endpoint();
        let mut serial_caps = [None; 16];
        serial_caps[1] = Some(ipc::Capability {
            kind: ipc::CapKind::Irq(4),
            rights: ipc::CAP_NOTIFY,
        });
        serial_caps[2] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(serial_ep),
            rights: ipc::CAP_READ | ipc::CAP_WRITE,
        });
        serial_caps[3] = Some(ipc::Capability {
            kind: ipc::CapKind::IoPort {
                base: 0x3F8,
                len: 8,
            },
            rights: ipc::CAP_READ | ipc::CAP_WRITE,
        });
        serial_caps[4] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(shell_in_ep),
            rights: ipc::CAP_WRITE,
        });
        let serial_elf = include_bytes!(concat!(env!("OUT_DIR"), "/serial.elf"));
        match user::launch(serial_elf, serial_caps) {
            Ok(tid) => crate::serial_println!("serial thread id = {}", tid),
            Err(e) => panic!("serial launch failed: {:?}", e),
        }

        // 6. PS/2 keyboard driver as a userspace server: IRQ1 + 0x60..0x64.
        let kbd_ep = ipc::create_endpoint();
        let mut kbd_caps = [None; 16];
        kbd_caps[1] = Some(ipc::Capability {
            kind: ipc::CapKind::Irq(1),
            rights: ipc::CAP_NOTIFY,
        });
        kbd_caps[2] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(kbd_ep),
            rights: ipc::CAP_READ | ipc::CAP_WRITE,
        });
        kbd_caps[3] = Some(ipc::Capability {
            kind: ipc::CapKind::IoPort { base: 0x60, len: 5 },
            rights: ipc::CAP_READ | ipc::CAP_WRITE,
        });
        let kbd_elf = include_bytes!(concat!(env!("OUT_DIR"), "/kbd.elf"));
        match user::launch(kbd_elf, kbd_caps) {
            Ok(tid) => crate::serial_println!("kbd thread id = {}", tid),
            Err(e) => panic!("kbd launch failed: {:?}", e),
        }

        crate::serial_println!("Milestone 6: userspace drivers spawned");

        // 6b. Framebuffer server. The kernel only passes a bounded MMIO
        //     capability and geometry over IPC; all pixels are drawn by the
        //     ring-3 fb server.
        if let Some(fb) = info.framebuffer {
            let fb_info_ep = ipc::create_endpoint();
            let fb_console_ep = ipc::create_endpoint();

            ipc::send(
                fb_info_ep,
                ipc::Message {
                    data: {
                        let mut data = [0u8; ipc::MAX_MSG_LEN];
                        data[..bootinfo::FB_INFO_LEN].copy_from_slice(&fb.info_message());
                        data
                    },
                    len: bootinfo::FB_INFO_LEN,
                    sender: 0,
                },
            );

            let mut fb_caps = [None; 16];
            fb_caps[1] = Some(ipc::Capability {
                kind: ipc::CapKind::Mmio {
                    paddr: fb.paddr,
                    len: fb.len,
                },
                rights: ipc::CAP_READ | ipc::CAP_WRITE,
            });
            fb_caps[2] = Some(ipc::Capability {
                kind: ipc::CapKind::Endpoint(fb_info_ep),
                rights: ipc::CAP_READ,
            });
            fb_caps[3] = Some(ipc::Capability {
                kind: ipc::CapKind::Endpoint(fb_console_ep),
                rights: ipc::CAP_READ,
            });

            let fb_elf = include_bytes!(concat!(env!("OUT_DIR"), "/fb.elf"));
            match user::launch(fb_elf, fb_caps) {
                Ok(tid) => {
                    crate::serial_println!("fb thread id = {}", tid);
                    console::attach(fb_console_ep);
                    crate::serial_println!("fb: graphical console attached");
                }
                Err(e) => crate::serial_println!("fb: launch failed: {:?}", e),
            }
        } else {
            crate::serial_println!("fb: graphical console disabled");
        }

        // 7. Init + shell servers. Replies from init reach the shell over
        //    their own endpoint, so nothing is shortcut through the kernel.
        let mut init_caps = [None; 16];
        init_caps[1] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(init_ep),
            rights: ipc::CAP_READ,
        });
        init_caps[2] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(shell_reply_ep),
            rights: ipc::CAP_WRITE,
        });
        let init_elf = include_bytes!(concat!(env!("OUT_DIR"), "/init.elf"));
        match user::launch(init_elf, init_caps) {
            Ok(tid) => crate::serial_println!("init thread id = {}", tid),
            Err(e) => panic!("init launch failed: {:?}", e),
        }

        let mut shell_caps = [None; 16];
        shell_caps[1] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(init_ep),
            rights: ipc::CAP_WRITE,
        });
        shell_caps[2] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(shell_in_ep),
            rights: ipc::CAP_READ,
        });
        shell_caps[3] = Some(ipc::Capability {
            kind: ipc::CapKind::Endpoint(shell_reply_ep),
            rights: ipc::CAP_READ,
        });
        let shell_elf = include_bytes!(concat!(env!("OUT_DIR"), "/shell.elf"));
        match user::launch(shell_elf, shell_caps) {
            Ok(tid) => crate::serial_println!("shell thread id = {}", tid),
            Err(e) => panic!("shell launch failed: {:?}", e),
        }

        crate::serial_println!("Milestone 7: init + shell spawned");

        // 7b. Virtio-net driver. The kernel only locates the PCI function
        //     and grants bounded I/O + DMA capabilities; the driver lives
        //     entirely in ring 3.
        match pci::find_virtio_net() {
            Some(dev) => {
                let mut net_caps = [None; 16];
                net_caps[1] = Some(ipc::Capability {
                    kind: ipc::CapKind::IoPort {
                        base: dev.io_base,
                        len: dev.io_len,
                    },
                    rights: ipc::CAP_READ | ipc::CAP_WRITE,
                });
                net_caps[2] = Some(ipc::Capability {
                    kind: ipc::CapKind::Dma { len: 64 * 1024 },
                    rights: ipc::CAP_READ | ipc::CAP_WRITE,
                });
                let net_elf = include_bytes!(concat!(env!("OUT_DIR"), "/net.elf"));
                match user::launch(net_elf, net_caps) {
                    Ok(tid) => crate::serial_println!("net thread id = {}", tid),
                    Err(e) => panic!("net launch failed: {:?}", e),
                }
                crate::serial_println!("Milestone 9: virtio-net driver spawned");
            }
            None => crate::serial_println!("Milestone 9: virtio-net not present (skipped)"),
        }

        // 8. Bring up the application processors, then prove that both
        //    CPUs run their own run queues concurrently.
        arch::smp::start_aps();
        spawn_smp_demo();
        crate::serial_println!(
            "Milestone 8: smp online ({} of {} cpu(s))",
            arch::smp::online_count(),
            arch::smp::cpu_count()
        );

        // WHY unsafe: all kernel structures are ready; enable interrupts.
        unsafe {
            asm!("sti", options(nomem, nostack, preserves_flags));
        }

        // Main thread is done; it becomes CPU0's idle fallback.
        sched::mark_self_idle();
        loop {
            // WHY unsafe: `hlt` waits for the next interrupt.
            unsafe {
                asm!("hlt", options(nomem, nostack, preserves_flags));
            }
        }
    }

    #[allow(unreachable_code)]
    halt_forever()
}

/// Park the current CPU forever with interrupts disabled.
pub fn halt_forever() -> ! {
    loop {
        // WHY unsafe: `hlt` is privileged; we're in ring 0 with IRQs off.
        unsafe {
            asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// ---- SMP smoke demo -------------------------------------------------------

/// Two kernel threads pinned to different CPUs. Each reports where it ran;
/// seeing `cpu 0` and `cpu 1` proves both processors are scheduling.
#[cfg(not(feature = "test"))]
fn spawn_smp_demo() {
    sched::spawn_on("smp-a", demo_a, 0);
    if arch::smp::cpu_count() > 1 {
        sched::spawn_on("smp-b", demo_b, 1);
    }
}

#[cfg(not(feature = "test"))]
extern "C" fn demo_a() {
    demo_worker("A");
}
#[cfg(not(feature = "test"))]
extern "C" fn demo_b() {
    demo_worker("B");
}

#[cfg(not(feature = "test"))]
fn demo_worker(name: &'static str) -> ! {
    let cpu = arch::smp::cpu_index();
    crate::serial_println!("smp: worker {} running on cpu {}", name, cpu);
    // Keep the thread alive long enough for the other CPU to pick up its
    // own worker, making the concurrency observable in the log order.
    for _ in 0..20_000_000u32 {
        core::hint::spin_loop();
    }
    sched::exit_current()
}

// ---- QEMU test exit -------------------------------------------------------

/// Exit codes understood by QEMU's `isa-debug-exit` device.
#[derive(Copy, Clone)]
pub enum QemuExitCode {
    /// 0x10 -> QEMU process exit status 33.
    Success = 0x10,
    /// 0x11 -> QEMU process exit status 35.
    Failed = 0x11,
}

/// Terminate QEMU through the `isa-debug-exit` port. Only valid when QEMU
/// was started with `-device isa-debug-exit,iobase=0xf4,iosize=0x04`.
pub fn exit_qemu(code: QemuExitCode) -> ! {
    // WHY unsafe: writing to the QEMU isa-debug-exit port. Only active
    // when QEMU is launched with -device isa-debug-exit,iobase=0xf4.
    unsafe {
        asm!("out dx, eax",
             in("dx") 0xf4u16,
             in("eax") code as u32,
             options(nomem, nostack, preserves_flags));
    }
    halt_forever()
}

// ---- Internal helpers -----------------------------------------------------

/// Run `f` with exclusive access to the global memory manager.
#[doc(hidden)]
pub fn memory_mut<R>(f: impl FnOnce(&mut MemoryManager) -> R) -> R {
    let mut guard = MEMORY.get().expect("MEMORY unset").lock();
    f(&mut guard)
}

/// `(total_frames, free_frames)`; used by the kernel self-tests.
#[doc(hidden)]
pub fn memory_init_state() -> MemoryResult<(u64, u64)> {
    memory_mut(|m| Ok((m.frames.total_frames(), m.frames.free_count())))
}
